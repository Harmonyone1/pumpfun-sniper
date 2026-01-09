//! Configuration for the adaptive filtering system

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::filter::scoring::ScoringThresholds;
use crate::filter::signals::SignalType;

/// Main configuration for adaptive filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveFilterConfig {
    /// Enable adaptive filtering (false = use basic filter only)
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Hot path configuration
    #[serde(default)]
    pub hot_path: HotPathConfig,

    /// Background enrichment configuration
    #[serde(default)]
    pub background: BackgroundConfig,

    /// Signal weights (overrides defaults)
    #[serde(default)]
    pub weights: HashMap<String, f64>,

    /// Scoring thresholds
    #[serde(default)]
    pub thresholds: ScoringThresholds,

    /// Position reassessment configuration
    #[serde(default)]
    pub reassessment: ReassessmentConfig,

    /// Cache configuration
    #[serde(default)]
    pub cache: CacheConfig,

    /// Known actors configuration
    #[serde(default)]
    pub known_actors: KnownActorsConfig,
}

fn default_enabled() -> bool {
    true
}

impl Default for AdaptiveFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hot_path: HotPathConfig::default(),
            background: BackgroundConfig::default(),
            weights: HashMap::new(),
            thresholds: ScoringThresholds::default(),
            reassessment: ReassessmentConfig::default(),
            cache: CacheConfig::default(),
            known_actors: KnownActorsConfig::default(),
        }
    }
}

impl AdaptiveFilterConfig {
    /// Parse signal weights from string keys to SignalType
    pub fn signal_weights(&self) -> HashMap<SignalType, f64> {
        let mut result = HashMap::new();

        for (key, &weight) in &self.weights {
            if let Some(signal_type) = Self::parse_signal_type(key) {
                result.insert(signal_type, weight);
            }
        }

        result
    }

    /// Parse a string key to SignalType
    fn parse_signal_type(key: &str) -> Option<SignalType> {
        match key.to_lowercase().as_str() {
            "wallet_age" => Some(SignalType::WalletAge),
            "wallet_history" => Some(SignalType::WalletHistory),
            "wallet_prior_performance" => Some(SignalType::WalletPriorPerformance),
            "known_deployer" => Some(SignalType::KnownDeployer),
            "known_sniper" => Some(SignalType::KnownSniper),
            "wallet_clustering" => Some(SignalType::WalletClustering),
            "supply_dispersion" => Some(SignalType::SupplyDispersion),
            "concentration_risk" => Some(SignalType::ConcentrationRisk),
            "early_accumulation" => Some(SignalType::EarlyAccumulation),
            "buy_timing" => Some(SignalType::BuyTiming),
            "sell_timing" => Some(SignalType::SellTiming),
            "burst_detection" => Some(SignalType::BurstDetection),
            "wash_trading" => Some(SignalType::WashTrading),
            "velocity_metrics" => Some(SignalType::VelocityMetrics),
            "transaction_size_ratio" => Some(SignalType::TransactionSizeRatio),
            "coordinated_funding" => Some(SignalType::CoordinatedFunding),
            "deployer_pattern" => Some(SignalType::DeployerPattern),
            "liquidity_seeding" => Some(SignalType::LiquiditySeeding),
            "early_sell_pressure" => Some(SignalType::EarlySellPressure),
            "organic_demand" => Some(SignalType::OrganicDemand),
            "name_quality" => Some(SignalType::NameQuality),
            "symbol_quality" => Some(SignalType::SymbolQuality),
            "uri_analysis" => Some(SignalType::UriAnalysis),
            _ => None,
        }
    }
}

/// Hot path configuration (fast signals only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotPathConfig {
    /// Maximum latency for hot path signals (ms)
    #[serde(default = "default_hot_path_latency")]
    pub max_latency_ms: u64,

    /// Providers to run in hot path
    #[serde(default = "default_hot_path_providers")]
    pub providers: Vec<String>,
}

fn default_hot_path_latency() -> u64 {
    50
}

fn default_hot_path_providers() -> Vec<String> {
    vec![
        "name_quality".to_string(),
        "known_actors".to_string(),
        "liquidity_threshold".to_string(),
    ]
}

impl Default for HotPathConfig {
    fn default() -> Self {
        Self {
            max_latency_ms: default_hot_path_latency(),
            providers: default_hot_path_providers(),
        }
    }
}

/// Background enrichment configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundConfig {
    /// Number of background workers
    #[serde(default = "default_worker_count")]
    pub worker_count: usize,

    /// Additional RPC endpoints for background workers
    #[serde(default)]
    pub rpc_endpoints: Vec<String>,

    /// Maximum latency for background signals (ms)
    #[serde(default = "default_background_latency")]
    pub max_latency_ms: u64,
}

fn default_worker_count() -> usize {
    4
}

fn default_background_latency() -> u64 {
    2000
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            worker_count: default_worker_count(),
            rpc_endpoints: Vec::new(),
            max_latency_ms: default_background_latency(),
        }
    }
}

/// Position reassessment configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReassessmentConfig {
    /// Enable continuous position monitoring
    #[serde(default = "default_reassessment_enabled")]
    pub enabled: bool,

    /// Check interval (seconds)
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,

    /// Rescore after significant events
    #[serde(default)]
    pub rescore_on_large_trade: bool,

    /// Large trade threshold (SOL)
    #[serde(default = "default_large_trade_threshold")]
    pub large_trade_threshold_sol: f64,

    /// Exit if score drops below this
    #[serde(default = "default_exit_score")]
    pub exit_on_score_below: f64,

    /// Exit if risk rises above this
    #[serde(default = "default_exit_risk")]
    pub exit_on_risk_above: f64,
}

fn default_reassessment_enabled() -> bool {
    true
}

fn default_interval_secs() -> u64 {
    30
}

fn default_large_trade_threshold() -> f64 {
    1.0
}

fn default_exit_score() -> f64 {
    -0.5
}

fn default_exit_risk() -> f64 {
    0.8
}

impl Default for ReassessmentConfig {
    fn default() -> Self {
        Self {
            enabled: default_reassessment_enabled(),
            interval_secs: default_interval_secs(),
            rescore_on_large_trade: false,
            large_trade_threshold_sol: default_large_trade_threshold(),
            exit_on_score_below: default_exit_score(),
            exit_on_risk_above: default_exit_risk(),
        }
    }
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Wallet cache size (entries)
    #[serde(default = "default_wallet_cache_size")]
    pub wallet_cache_size: usize,

    /// Wallet cache TTL (seconds)
    #[serde(default = "default_wallet_cache_ttl")]
    pub wallet_cache_ttl_secs: u64,

    /// Trade flow buffer size per token
    #[serde(default = "default_trade_flow_buffer")]
    pub trade_flow_buffer_size: usize,
}

fn default_wallet_cache_size() -> usize {
    10_000
}

fn default_wallet_cache_ttl() -> u64 {
    3600
}

fn default_trade_flow_buffer() -> usize {
    1000
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            wallet_cache_size: default_wallet_cache_size(),
            wallet_cache_ttl_secs: default_wallet_cache_ttl(),
            trade_flow_buffer_size: default_trade_flow_buffer(),
        }
    }
}

/// Known actors list configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownActorsConfig {
    /// Path to deployers file
    #[serde(default = "default_deployers_file")]
    pub deployers_file: String,

    /// Path to snipers file
    #[serde(default = "default_snipers_file")]
    pub snipers_file: String,

    /// Path to trusted wallets file
    #[serde(default = "default_trusted_file")]
    pub trusted_file: String,

    /// Refresh interval (seconds)
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
}

fn default_deployers_file() -> String {
    "data/known_deployers.txt".to_string()
}

fn default_snipers_file() -> String {
    "data/known_snipers.txt".to_string()
}

fn default_trusted_file() -> String {
    "data/trusted_wallets.txt".to_string()
}

fn default_refresh_interval() -> u64 {
    3600
}

impl Default for KnownActorsConfig {
    fn default() -> Self {
        Self {
            deployers_file: default_deployers_file(),
            snipers_file: default_snipers_file(),
            trusted_file: default_trusted_file(),
            refresh_interval_secs: default_refresh_interval(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AdaptiveFilterConfig::default();
        assert!(config.enabled);
        assert_eq!(config.hot_path.max_latency_ms, 50);
        assert_eq!(config.background.worker_count, 4);
    }

    #[test]
    fn test_signal_weights_parsing() {
        let mut config = AdaptiveFilterConfig::default();
        config.weights.insert("known_deployer".to_string(), 2.5);
        config.weights.insert("name_quality".to_string(), 0.3);
        config.weights.insert("invalid_signal".to_string(), 1.0);

        let parsed = config.signal_weights();
        assert_eq!(parsed.get(&SignalType::KnownDeployer), Some(&2.5));
        assert_eq!(parsed.get(&SignalType::NameQuality), Some(&0.3));
        assert!(!parsed.contains_key(&SignalType::WalletAge)); // Not in config
    }

    #[test]
    fn test_config_serde() {
        let config = AdaptiveFilterConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AdaptiveFilterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.enabled, config.enabled);
    }
}
