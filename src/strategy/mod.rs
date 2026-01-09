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
pub mod fatal_risk;
pub mod liquidity;
pub mod creator_privileges;
pub mod portfolio_risk;
pub mod arbitrator;

// Intelligence (P1)
pub mod delta_tracker;
pub mod price_action;
pub mod regime;
pub mod chain_health;
pub mod execution_feedback;

// Strategy (P1)
pub mod engine;
pub mod sizing;
pub mod exit_manager;
pub mod randomization;

// Tactics (P2)
pub mod tactics;

// Re-exports
pub use types::*;
pub use fatal_risk::{FatalRisk, FatalRiskEngine, FatalRiskContext};
pub use liquidity::{LiquidityAnalysis, LiquidityAnalyzer};
pub use creator_privileges::{CreatorPrivileges, CreatorPrivilegeChecker, Privilege};
pub use portfolio_risk::{PortfolioRiskGovernor, PortfolioState, PortfolioBlock};
pub use arbitrator::DecisionArbitrator;
pub use delta_tracker::{DeltaTracker, DeltaMetrics, RollingWindow};
pub use price_action::{PriceAction, PriceActionAnalyzer};
pub use regime::{RegimeClassifier, RegimeClassification, OrderFlowAnalysis, TokenDistribution, CreatorBehavior};
pub use chain_health::{ChainHealth, ChainState};
pub use execution_feedback::{ExecutionFeedback, ExecutionQuality};
pub use engine::{StrategyEngine, StrategyEngineConfig, TokenAnalysisContext, EntryEvaluation, PositionEvaluation};
pub use sizing::{PositionSizer, PositionSizingConfig, SizingContext};
pub use exit_manager::{ExitManager, ExitManagerConfig, PositionContext};
pub use randomization::{Randomizer, RandomizationConfig};
pub use tactics::{
    FrontRunDetector, AccumulationSignal,
    RugPredictor, RugPrediction, RugWarningSignal,
    SniperPiggyback, PiggybackSignal, SniperStat,
};
