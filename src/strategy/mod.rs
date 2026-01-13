//! Aggressive Trading Strategy System
//!
//! This module provides a comprehensive, multi-strategy trading system for pump.fun
//! with the following components:
//!
//! ## Core Safety (P0)
//! - `fatal_risk` - Kill switch with fatal flags
//! - `liquidity` - Slippage calculation & exit feasibility
//! - `creator_privileges` - Authority checking
//! - `portfolio_risk` - Global capital control
//! - `arbitrator` - Decision conflict resolution
//!
//! ## Intelligence (P1)
//! - `delta_tracker` - Rolling windows & trend detection
//! - `price_action` - VWAP, structure, volatility
//! - `regime` - Regime classification
//! - `chain_health` - Solana congestion monitoring
//! - `execution_feedback` - Fill quality tracking
//!
//! ## Strategy (P1)
//! - `engine` - Strategy coordinator
//! - `sizing` - Dynamic position calculator
//! - `exit_manager` - Adaptive exit selection
//! - `randomization` - Adversarial resistance
//!
//! ## Tactics (P2)
//! - `tactics` - Cunning tactics (frontrun, rug_predict, piggyback)

// Shared types
pub mod types;

// Core Safety (P0)
pub mod arbitrator;
pub mod creator_privileges;
pub mod fatal_risk;
pub mod liquidity;
pub mod portfolio_risk;

// Intelligence (P1)
pub mod chain_health;
pub mod delta_tracker;
pub mod execution_feedback;
pub mod price_action;
pub mod regime;

// Strategy (P1)
pub mod engine;
pub mod exit_manager;
pub mod randomization;
pub mod sizing;

// Tactics (P2)
pub mod tactics;

// Re-exports
pub use arbitrator::DecisionArbitrator;
pub use chain_health::{ChainHealth, ChainState};
pub use creator_privileges::{CreatorPrivilegeChecker, CreatorPrivileges, Privilege};
pub use delta_tracker::{DeltaMetrics, DeltaTracker, RollingWindow};
pub use engine::{
    EntryEvaluation, PositionEvaluation, StrategyEngine, StrategyEngineConfig, TokenAnalysisContext,
};
pub use execution_feedback::{ExecutionFeedback, ExecutionQuality};
pub use exit_manager::{ExitManager, ExitManagerConfig, PositionContext};
pub use fatal_risk::{FatalRisk, FatalRiskContext, FatalRiskEngine};
pub use liquidity::{LiquidityAnalysis, LiquidityAnalyzer};
pub use portfolio_risk::{PortfolioBlock, PortfolioRiskGovernor, PortfolioState};
pub use price_action::{PriceAction, PriceActionAnalyzer};
pub use randomization::{RandomizationConfig, Randomizer};
pub use regime::{
    CreatorBehavior, OrderFlowAnalysis, RegimeClassification, RegimeClassifier, TokenDistribution,
};
pub use sizing::{PositionSizer, PositionSizingConfig, SizingContext};
pub use tactics::{
    AccumulationSignal, FrontRunDetector, PiggybackSignal, RugPrediction, RugPredictor,
    RugWarningSignal, SniperPiggyback, SniperStat,
};
pub use types::*;
