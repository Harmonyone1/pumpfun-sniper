//! Shared types for the aggressive trading strategy system
//!
//! This module contains all shared types used across the strategy modules.

use serde::{Deserialize, Serialize};

/// Trading strategy types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradingStrategy {
    /// Ride volume spikes
    MomentumSurfing,
    /// Copy profitable wallets
    WhaleFollowing,
    /// Fast entry/exit on new tokens
    SnipeAndScalp,
    /// Adaptive strategy selection based on regime
    Adaptive,
}

impl Default for TradingStrategy {
    fn default() -> Self {
        Self::Adaptive
    }
}

impl std::fmt::Display for TradingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradingStrategy::MomentumSurfing => write!(f, "Momentum"),
            TradingStrategy::WhaleFollowing => write!(f, "Whale"),
            TradingStrategy::SnipeAndScalp => write!(f, "Snipe"),
            TradingStrategy::Adaptive => write!(f, "Adaptive"),
        }
    }
}

/// Token regime classification
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenRegime {
    /// Organic buying pressure with real demand
    OrganicPump {
        confidence: f64,
        expected_duration_secs: u64,
    },
    /// Snipers accumulated early, expecting quick dump
    SniperFlip {
        sniper_count: u32,
        expected_dump_in_secs: u64,
    },
    /// Fake volume from wash trading
    WashTrade {
        wash_pct: f64,
        real_volume_sol: f64,
    },
    /// Creator slowly selling holdings
    DeployerBleed {
        deployer_holdings_pct: f64,
        avg_sell_interval_secs: u64,
    },
    /// Unknown regime (insufficient data)
    Unknown {
        data_completeness: f64,
    },
}

impl Default for TokenRegime {
    fn default() -> Self {
        Self::Unknown {
            data_completeness: 0.0,
        }
    }
}

impl TokenRegime {
    /// Returns true if this regime should be avoided
    pub fn should_avoid(&self) -> bool {
        matches!(self, TokenRegime::WashTrade { .. } | TokenRegime::DeployerBleed { .. })
    }

    /// Returns the confidence level for this regime
    pub fn confidence(&self) -> f64 {
        match self {
            TokenRegime::OrganicPump { confidence, .. } => *confidence,
            TokenRegime::SniperFlip { .. } => 0.7,
            TokenRegime::WashTrade { wash_pct, .. } => *wash_pct,
            TokenRegime::DeployerBleed { .. } => 0.8,
            TokenRegime::Unknown { data_completeness } => *data_completeness,
        }
    }

    /// Returns the position size multiplier for this regime
    pub fn size_multiplier(&self) -> f64 {
        match self {
            TokenRegime::OrganicPump { confidence, .. } if *confidence > 0.8 => 1.5,
            TokenRegime::OrganicPump { .. } => 1.0,
            TokenRegime::SniperFlip { .. } => 0.3,
            TokenRegime::WashTrade { .. } => 0.0,
            TokenRegime::DeployerBleed { .. } => 0.0,
            TokenRegime::Unknown { .. } => 0.5,
        }
    }
}

/// Exit style types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStyle {
    /// Quick scalp with fixed target
    QuickScalp {
        target_pct: f64,
    },
    /// Tiered exit at multiple levels
    TieredExit {
        /// (pct_gain, pct_to_sell)
        levels: Vec<(f64, f64)>,
    },
    /// Trailing stop after activation
    TrailingStop {
        trail_pct: f64,
        activation_pct: f64,
    },
    /// Exit after max hold time
    TimeBased {
        max_hold_secs: u64,
    },
    /// Exit on specific conditions
    ConditionBased {
        exit_on: Vec<ExitCondition>,
    },
}

impl Default for ExitStyle {
    fn default() -> Self {
        Self::QuickScalp { target_pct: 25.0 }
    }
}

/// Exit conditions for condition-based exits
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitCondition {
    /// Volume drops significantly
    MomentumFade,
    /// Tracked whale sells
    WhaleExits,
    /// Top holder percentage increases
    DistributionWorsens,
    /// Creator starts dumping
    CreatorSelling,
    /// Lower high formed (bearish structure)
    PriceStructureBreaks,
    /// Stop loss hit
    StopLossHit,
    /// Max hold time reached
    MaxHoldTimeReached,
}

/// Entry signal from strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySignal {
    pub mint: String,
    pub strategy: TradingStrategy,
    pub confidence: f64,
    pub suggested_size_sol: f64,
    pub urgency: Urgency,
    pub max_price: Option<f64>,
    pub reason: String,
}

/// Urgency level for entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// Execute immediately
    Immediate,
    /// Execute within seconds
    High,
    /// Can wait for better price
    Normal,
    /// Low priority, wait for conditions
    Low,
}

/// Exit signal from strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitSignal {
    pub mint: String,
    pub pct_to_sell: f64,
    pub reason: ExitReason,
    pub urgency: Urgency,
}

/// Reason for exit
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit { pnl_pct: f64 },
    TrailingStopHit { peak_pnl_pct: f64, current_pnl_pct: f64 },
    StopLoss { loss_pct: f64 },
    MomentumFade,
    WhaleExited { whale_address: String },
    RugPredicted { probability: f64 },
    CreatorSelling { pct_sold: f64 },
    MaxHoldTime { held_secs: u64 },
    FatalRisk { risk: String },
    ManualExit,
}

/// Trading action from arbitrator
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradingAction {
    Enter {
        mint: String,
        size_sol: f64,
        strategy: TradingStrategy,
    },
    Exit {
        mint: String,
        pct: f64,
        reason: String,
    },
    Hold,
    Skip {
        reason: String,
    },
    FatalReject {
        reason: String,
    },
    Pause {
        reason: String,
        resume_after_secs: u64,
    },
}

/// Decision source for arbitration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    FatalRisk,
    ChainHealth,
    PortfolioRisk,
    RugPredictor,
    ExitManager,
    Strategy,
    RegimeOptimization,
}

impl DecisionSource {
    /// Returns the priority (lower = higher priority)
    pub fn priority(&self) -> u8 {
        match self {
            DecisionSource::FatalRisk => 0,
            DecisionSource::ChainHealth => 1,
            DecisionSource::PortfolioRisk => 2,
            DecisionSource::RugPredictor => 3,
            DecisionSource::ExitManager => 4,
            DecisionSource::Strategy => 5,
            DecisionSource::RegimeOptimization => 6,
        }
    }
}

/// Arbitrated decision with audit trail
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitratedDecision {
    pub action: TradingAction,
    pub source: DecisionSource,
    /// (source, original_action_description, override_reason)
    pub overridden: Vec<(DecisionSource, String, String)>,
    pub confidence: f64,
}

/// Trend direction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    StronglyImproving,
    Improving,
    Stable,
    Deteriorating,
    StronglyDeteriorating,
}

impl Default for Trend {
    fn default() -> Self {
        Self::Stable
    }
}

impl Trend {
    /// Convert from slope value
    pub fn from_slope(slope: f64, threshold: f64) -> Self {
        if slope > threshold * 2.0 {
            Trend::StronglyImproving
        } else if slope > threshold {
            Trend::Improving
        } else if slope < -threshold * 2.0 {
            Trend::StronglyDeteriorating
        } else if slope < -threshold {
            Trend::Deteriorating
        } else {
            Trend::Stable
        }
    }

    /// Returns true if trend is positive
    pub fn is_positive(&self) -> bool {
        matches!(self, Trend::StronglyImproving | Trend::Improving)
    }

    /// Returns true if trend is negative
    pub fn is_negative(&self) -> bool {
        matches!(self, Trend::StronglyDeteriorating | Trend::Deteriorating)
    }
}

/// Congestion level for chain health
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CongestionLevel {
    /// Green light - proceed normally
    Normal,
    /// Proceed with caution
    Elevated,
    /// Reduce position sizes
    High,
    /// Pause new entries
    Severe,
    /// Exit-only mode
    Critical,
}

impl Default for CongestionLevel {
    fn default() -> Self {
        Self::Normal
    }
}

/// Chain action recommendation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainAction {
    ProceedNormally,
    ReducePositionSize { factor: f64 },
    IncreasePriorityFee { to_lamports: u64 },
    PauseNewEntries,
    ExitOnlyMode,
}

impl Default for ChainAction {
    fn default() -> Self {
        Self::ProceedNormally
    }
}

/// Position tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub mint: String,
    pub entry_price: f64,
    pub entry_time: chrono::DateTime<chrono::Utc>,
    pub size_sol: f64,
    pub tokens_held: u64,
    pub strategy: TradingStrategy,
    pub exit_style: ExitStyle,
    pub highest_price: f64,
    pub lowest_price: f64,
    pub exit_levels_hit: Vec<f64>,
}

impl Position {
    /// Calculate current PnL percentage
    pub fn pnl_pct(&self, current_price: f64) -> f64 {
        if self.entry_price == 0.0 {
            return 0.0;
        }
        ((current_price - self.entry_price) / self.entry_price) * 100.0
    }

    /// Calculate current PnL in SOL
    pub fn pnl_sol(&self, current_price: f64) -> f64 {
        let current_value = (self.tokens_held as f64) * current_price;
        current_value - self.size_sol
    }

    /// Update price tracking
    pub fn update_price(&mut self, price: f64) {
        if price > self.highest_price {
            self.highest_price = price;
        }
        if price < self.lowest_price || self.lowest_price == 0.0 {
            self.lowest_price = price;
        }
    }

    /// Get hold duration
    pub fn hold_duration(&self) -> chrono::Duration {
        chrono::Utc::now() - self.entry_time
    }
}

/// Side for execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// Execution record for feedback
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub mint: String,
    pub side: Side,
    pub requested_size_sol: f64,
    pub filled_size_sol: f64,
    pub expected_price: f64,
    pub actual_price: f64,
    pub slippage_pct: f64,
    pub latency_ms: u64,
    pub success: bool,
    pub failure_reason: Option<String>,
    pub tx_signature: Option<String>,
}

impl ExecutionRecord {
    /// Create a successful execution record
    pub fn success(
        mint: String,
        side: Side,
        requested_size_sol: f64,
        filled_size_sol: f64,
        expected_price: f64,
        actual_price: f64,
        latency_ms: u64,
        tx_signature: String,
    ) -> Self {
        let slippage_pct = if expected_price != 0.0 {
            ((actual_price - expected_price) / expected_price) * 100.0
        } else {
            0.0
        };

        Self {
            timestamp: chrono::Utc::now(),
            mint,
            side,
            requested_size_sol,
            filled_size_sol,
            expected_price,
            actual_price,
            slippage_pct,
            latency_ms,
            success: true,
            failure_reason: None,
            tx_signature: Some(tx_signature),
        }
    }

    /// Create a failed execution record
    pub fn failure(
        mint: String,
        side: Side,
        requested_size_sol: f64,
        expected_price: f64,
        latency_ms: u64,
        reason: String,
    ) -> Self {
        Self {
            timestamp: chrono::Utc::now(),
            mint,
            side,
            requested_size_sol,
            filled_size_sol: 0.0,
            expected_price,
            actual_price: 0.0,
            slippage_pct: 0.0,
            latency_ms,
            success: false,
            failure_reason: Some(reason),
            tx_signature: None,
        }
    }
}

/// Full decision explanation for logging
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionExplanation {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub mint: String,
    pub final_score: f64,
    pub action: TradingAction,

    // Attribution
    pub top_contributing_signals: Vec<(String, f64)>,
    pub top_risk_factors: Vec<(String, f64)>,
    pub regime: TokenRegime,
    pub regime_confidence: f64,

    // Data quality
    pub data_completeness: f64,
    pub missing_data: Vec<String>,

    // Strategy selection
    pub selected_strategy: TradingStrategy,
    pub strategy_reason: String,
    pub position_size_sol: f64,
    pub exit_style: ExitStyle,

    // Arbitration audit trail
    pub decision_source: DecisionSource,
    pub overridden_signals: Vec<(DecisionSource, String)>,

    // Portfolio state
    pub open_position_count: usize,
    pub total_exposure_sol: f64,
    pub portfolio_block_reason: Option<String>,

    // Chain conditions
    pub chain_congestion: CongestionLevel,
    pub chain_action_taken: ChainAction,

    // Execution quality
    pub recent_slippage_avg: f64,
    pub confidence_adjustment: f64,

    // Randomization applied
    pub entry_delay_applied_ms: u64,
    pub size_jitter_applied_pct: f64,
}

impl Default for DecisionExplanation {
    fn default() -> Self {
        Self {
            timestamp: chrono::Utc::now(),
            mint: String::new(),
            final_score: 0.0,
            action: TradingAction::Hold,
            top_contributing_signals: vec![],
            top_risk_factors: vec![],
            regime: TokenRegime::default(),
            regime_confidence: 0.0,
            data_completeness: 0.0,
            missing_data: vec![],
            selected_strategy: TradingStrategy::default(),
            strategy_reason: String::new(),
            position_size_sol: 0.0,
            exit_style: ExitStyle::default(),
            decision_source: DecisionSource::Strategy,
            overridden_signals: vec![],
            open_position_count: 0,
            total_exposure_sol: 0.0,
            portfolio_block_reason: None,
            chain_congestion: CongestionLevel::default(),
            chain_action_taken: ChainAction::default(),
            recent_slippage_avg: 0.0,
            confidence_adjustment: 0.0,
            entry_delay_applied_ms: 0,
            size_jitter_applied_pct: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regime_should_avoid() {
        assert!(!TokenRegime::OrganicPump {
            confidence: 0.8,
            expected_duration_secs: 60
        }
        .should_avoid());
        assert!(TokenRegime::WashTrade {
            wash_pct: 0.7,
            real_volume_sol: 1.0
        }
        .should_avoid());
        assert!(TokenRegime::DeployerBleed {
            deployer_holdings_pct: 40.0,
            avg_sell_interval_secs: 30
        }
        .should_avoid());
    }

    #[test]
    fn test_regime_size_multiplier() {
        assert_eq!(
            TokenRegime::OrganicPump {
                confidence: 0.9,
                expected_duration_secs: 60
            }
            .size_multiplier(),
            1.5
        );
        assert_eq!(
            TokenRegime::SniperFlip {
                sniper_count: 5,
                expected_dump_in_secs: 30
            }
            .size_multiplier(),
            0.3
        );
        assert_eq!(
            TokenRegime::WashTrade {
                wash_pct: 0.8,
                real_volume_sol: 0.5
            }
            .size_multiplier(),
            0.0
        );
    }

    #[test]
    fn test_decision_source_priority() {
        assert!(DecisionSource::FatalRisk.priority() < DecisionSource::PortfolioRisk.priority());
        assert!(DecisionSource::PortfolioRisk.priority() < DecisionSource::Strategy.priority());
    }

    #[test]
    fn test_trend_from_slope() {
        assert_eq!(Trend::from_slope(0.5, 0.1), Trend::StronglyImproving);
        assert_eq!(Trend::from_slope(0.15, 0.1), Trend::Improving);
        assert_eq!(Trend::from_slope(0.0, 0.1), Trend::Stable);
        assert_eq!(Trend::from_slope(-0.15, 0.1), Trend::Deteriorating);
        assert_eq!(Trend::from_slope(-0.5, 0.1), Trend::StronglyDeteriorating);
    }

    #[test]
    fn test_position_pnl() {
        let pos = Position {
            mint: "test".to_string(),
            entry_price: 0.001,
            entry_time: chrono::Utc::now(),
            size_sol: 0.1,
            tokens_held: 100_000,
            strategy: TradingStrategy::SnipeAndScalp,
            exit_style: ExitStyle::default(),
            highest_price: 0.001,
            lowest_price: 0.001,
            exit_levels_hit: vec![],
        };

        // 50% gain
        assert!((pos.pnl_pct(0.0015) - 50.0).abs() < 0.01);
        // 20% loss
        assert!((pos.pnl_pct(0.0008) - (-20.0)).abs() < 0.01);
    }
}
