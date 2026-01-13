//! Smart Money Detection Module
//!
//! This module provides wallet profiling and elite wallet detection:
//! - P&L calculation from transaction history
//! - Alpha Score computation for wallet quality
//! - Wallet categorization (True Signal, Bundled/Team, MEV bots)
//! - Wallet clustering by funding relationships

pub mod alpha_score;
pub mod clustering;
pub mod wallet_profiler;

pub use alpha_score::{AlphaScore, WalletCategory};
pub use clustering::{ClusteringStats, WalletCluster, WalletClusterConfig, WalletClusterer};
pub use wallet_profiler::{WalletProfile, WalletProfiler, WalletProfilerConfig};
