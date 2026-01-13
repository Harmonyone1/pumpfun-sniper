//! Token filtering module
//!
//! This module provides both basic regex/threshold filtering and
//! advanced adaptive filtering with multi-signal scoring.

// Core filtering (existing)
pub mod holder_watcher;
pub mod kill_switch;
pub mod token_filter;
pub mod wallet_tracker;

// Adaptive filtering system (new)
pub mod adaptive;
pub mod bundled_detection;
pub mod cache;
pub mod enrichment;
pub mod helius;
pub mod momentum;
pub mod scoring;
pub mod signals;
pub mod smart_money;
pub mod types;

// Re-exports for basic filtering
pub use holder_watcher::{AlertUrgency, HolderSellAlert, HolderWatcher, HolderWatcherConfig};
pub use kill_switch::{
    DeployerTracker, KillSwitchAlert, KillSwitchConfig, KillSwitchDecision,
    KillSwitchEvaluator, KillSwitchType, KillSwitchUrgency,
};
pub use token_filter::TokenFilter;
pub use wallet_tracker::WalletTracker;

// Re-exports for adaptive filtering
pub use adaptive::{AdaptiveFilter, AdaptiveFilterConfig};
pub use cache::FilterCache;
pub use enrichment::{
    create_enrichment_system, EnrichmentConfig, EnrichmentHandle, EnrichmentPriority,
    EnrichmentService, EnrichmentWorker,
};
pub use bundled_detection::{
    BundleDetectionReason, BundleGroup, BundleSellAlert, BundledDetectionConfig, BundledDetector,
    EarlyBuy,
};
pub use helius::{HeliusClient, MintInfo, SolTransfer};
pub use momentum::{MomentumConfig, MomentumMetrics, MomentumStatus, MomentumValidator};
pub use scoring::{
    ReadinessState, Recommendation, ScoringEngine, ScoringResult, ScoringThresholds,
};
pub use signals::{
    MetadataSignalProvider, Signal, SignalProvider, SignalType, SmartMoneySignalProvider,
    WalletBehaviorSignalProvider,
};
pub use smart_money::{
    AlphaScore, ClusteringStats, WalletCategory, WalletCluster, WalletClusterConfig,
    WalletClusterer, WalletProfile, WalletProfiler, WalletProfilerConfig,
};
pub use types::{SignalContext, TokenHolderInfo, WalletHistory, WalletTrade};
