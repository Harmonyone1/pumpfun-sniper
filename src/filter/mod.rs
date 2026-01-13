//! Token filtering module
//!
//! This module provides both basic regex/threshold filtering and
//! advanced adaptive filtering with multi-signal scoring.

// Core filtering (existing)
pub mod token_filter;
pub mod wallet_tracker;
pub mod holder_watcher;

// Adaptive filtering system (new)
pub mod adaptive;
pub mod cache;
pub mod enrichment;
pub mod helius;
pub mod momentum;
pub mod scoring;
pub mod signals;
pub mod types;

// Re-exports for basic filtering
pub use token_filter::TokenFilter;
pub use wallet_tracker::WalletTracker;
pub use holder_watcher::{HolderWatcher, HolderWatcherConfig, HolderSellAlert, AlertUrgency};

// Re-exports for adaptive filtering
pub use adaptive::{AdaptiveFilter, AdaptiveFilterConfig};
pub use cache::FilterCache;
pub use enrichment::{
    create_enrichment_system, EnrichmentConfig, EnrichmentHandle, EnrichmentPriority,
    EnrichmentService, EnrichmentWorker,
};
pub use helius::{HeliusClient, MintInfo};
pub use scoring::{Recommendation, ReadinessState, ScoringEngine, ScoringResult, ScoringThresholds};
pub use signals::{MetadataSignalProvider, Signal, SignalProvider, SignalType, WalletBehaviorSignalProvider};
pub use types::{SignalContext, TokenHolderInfo, WalletHistory, WalletTrade};
pub use momentum::{MomentumValidator, MomentumConfig, MomentumStatus, MomentumMetrics};
