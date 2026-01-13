//! Signal providers for the adaptive filtering system
//!
//! Each signal provider computes one or more signals from token/trade data.
//! Signals are combined by the scoring engine to produce buy/skip decisions.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

use crate::filter::types::{PositionSignalContext, SignalContext, TradeSignalContext};

// Signal providers
pub mod metadata;
pub mod wallet_behavior;
// pub mod distribution;
// pub mod order_flow;
// pub mod wallet_profile;
// pub mod pumpfun_specific;

// Re-exports
pub use metadata::MetadataSignalProvider;
pub use wallet_behavior::WalletBehaviorSignalProvider;

/// Signal value range: -1.0 (extreme risk) to +1.0 (extreme opportunity)
pub type SignalValue = f64;

/// Confidence in a signal: 0.0 (no confidence) to 1.0 (full confidence)
pub type Confidence = f64;

/// Categories of signals for organization and weighting
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    // === Wallet Behavior Signals ===
    /// Age of the creator wallet (older = safer)
    WalletAge,
    /// Historical trading activity patterns
    WalletHistory,
    /// Prior performance (win rate, avg profit)
    WalletPriorPerformance,
    /// Matches known rug deployer blacklist
    KnownDeployer,
    /// Matches known sniper bot list
    KnownSniper,
    /// Part of a coordinated wallet cluster
    WalletClustering,

    // === Token Distribution Signals ===
    /// How spread out token holdings are
    SupplyDispersion,
    /// Risk from concentrated holdings (Gini coefficient)
    ConcentrationRisk,
    /// Suspicious early accumulation patterns
    EarlyAccumulation,

    // === Order Flow Signals ===
    /// Timing of first buys after launch
    BuyTiming,
    /// Early sell pressure detection
    SellTiming,
    /// Abnormal burst of transactions
    BurstDetection,
    /// Same wallets buying and selling (wash trading)
    WashTrading,
    /// Volume and trade velocity changes
    VelocityMetrics,

    // === Wallet Profiling Signals ===
    /// Transaction sizes relative to wallet balance
    TransactionSizeRatio,
    /// Coordinated funding patterns between wallets
    CoordinatedFunding,

    // === Pump.fun Specific Signals ===
    /// Creator's prior token deployment history
    DeployerPattern,
    /// Initial liquidity seeding behavior
    LiquiditySeeding,
    /// Sell pressure in first few minutes
    EarlySellPressure,
    /// Sustained organic demand vs artificial pumping
    OrganicDemand,

    // === Token Metadata Signals ===
    /// Token name quality/heuristics
    NameQuality,
    /// Symbol analysis
    SymbolQuality,
    /// Metadata URI patterns
    UriAnalysis,

    // === Token Authority Signals ===
    /// Mint authority status (can creator mint more tokens?)
    MintAuthority,
    /// Freeze authority status (can creator freeze accounts?)
    FreezeAuthority,

    // === Holder Distribution Signals ===
    /// Token holder concentration analysis
    HolderConcentration,
}

impl SignalType {
    /// Returns true if this signal can be computed in the hot path (<50ms)
    pub fn is_hot_path(&self) -> bool {
        matches!(
            self,
            SignalType::KnownDeployer
                | SignalType::KnownSniper
                | SignalType::NameQuality
                | SignalType::SymbolQuality
                | SignalType::UriAnalysis
                | SignalType::LiquiditySeeding
                | SignalType::WalletAge         // Only if cached
                | SignalType::MintAuthority     // If cached from Helius
                | SignalType::FreezeAuthority   // If cached from Helius
                | SignalType::HolderConcentration // If cached from Helius
                | SignalType::WalletHistory     // If cached from Helius
        )
    }

    /// Default weight for this signal type
    pub fn default_weight(&self) -> f64 {
        match self {
            // High weight - known bad actors
            SignalType::KnownDeployer => 2.0,
            SignalType::WashTrading => 1.8,
            SignalType::KnownSniper => 1.5,
            SignalType::ConcentrationRisk => 1.5,
            SignalType::DeployerPattern => 1.5,
            SignalType::CoordinatedFunding => 1.5,
            SignalType::WalletPriorPerformance => 1.5,
            SignalType::EarlySellPressure => 1.4,
            SignalType::BurstDetection => 1.3,
            SignalType::EarlyAccumulation => 1.2,
            SignalType::WalletAge => 1.2,
            SignalType::LiquiditySeeding => 1.2,

            // Normal weight
            SignalType::WalletHistory => 1.0,
            SignalType::WalletClustering => 1.0,
            SignalType::SupplyDispersion => 1.0,
            SignalType::BuyTiming => 1.0,
            SignalType::SellTiming => 1.0,
            SignalType::VelocityMetrics => 1.0,
            SignalType::OrganicDemand => 1.0,
            SignalType::TransactionSizeRatio => 0.8,

            // Lower weight - metadata signals (less reliable)
            SignalType::NameQuality => 0.5,
            SignalType::SymbolQuality => 0.3,
            SignalType::UriAnalysis => 0.4,

            // CRITICAL - Token authority signals
            SignalType::MintAuthority => 2.5,    // Can mint more = instant rug
            SignalType::FreezeAuthority => 2.0,  // Can freeze accounts

            // Holder distribution signals
            SignalType::HolderConcentration => 1.5,
        }
    }

    /// Category for grouping signals
    pub fn category(&self) -> SignalCategory {
        match self {
            SignalType::WalletAge
            | SignalType::WalletHistory
            | SignalType::WalletPriorPerformance
            | SignalType::KnownDeployer
            | SignalType::KnownSniper
            | SignalType::WalletClustering => SignalCategory::WalletBehavior,

            SignalType::SupplyDispersion
            | SignalType::ConcentrationRisk
            | SignalType::EarlyAccumulation => SignalCategory::Distribution,

            SignalType::BuyTiming
            | SignalType::SellTiming
            | SignalType::BurstDetection
            | SignalType::WashTrading
            | SignalType::VelocityMetrics => SignalCategory::OrderFlow,

            SignalType::TransactionSizeRatio | SignalType::CoordinatedFunding => {
                SignalCategory::WalletProfile
            }

            SignalType::DeployerPattern
            | SignalType::LiquiditySeeding
            | SignalType::EarlySellPressure
            | SignalType::OrganicDemand => SignalCategory::PumpfunSpecific,

            SignalType::NameQuality | SignalType::SymbolQuality | SignalType::UriAnalysis => {
                SignalCategory::Metadata
            }

            SignalType::MintAuthority | SignalType::FreezeAuthority => {
                SignalCategory::Distribution // Authority signals relate to token control
            }

            SignalType::HolderConcentration => SignalCategory::Distribution,
        }
    }
}

impl fmt::Display for SignalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Signal categories for grouping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalCategory {
    WalletBehavior,
    Distribution,
    OrderFlow,
    WalletProfile,
    PumpfunSpecific,
    Metadata,
}

/// A computed signal with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// Unique identifier for this signal type
    pub signal_type: SignalType,
    /// The computed value (-1.0 = extreme risk, +1.0 = extreme opportunity)
    pub value: SignalValue,
    /// Confidence in this signal (0.0 to 1.0)
    pub confidence: Confidence,
    /// Weight multiplier for scoring (from config)
    pub weight: f64,
    /// Human-readable explanation
    pub reason: String,
    /// Time to compute this signal
    #[serde(with = "duration_millis")]
    pub latency: Duration,
    /// Whether this was from cache
    pub cached: bool,
}

impl Signal {
    /// Create a new signal
    pub fn new(
        signal_type: SignalType,
        value: SignalValue,
        confidence: Confidence,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            signal_type,
            value: value.clamp(-1.0, 1.0),
            confidence: confidence.clamp(0.0, 1.0),
            weight: signal_type.default_weight(),
            reason: reason.into(),
            latency: Duration::ZERO,
            cached: false,
        }
    }

    /// Create a neutral signal (no effect on scoring)
    pub fn neutral(signal_type: SignalType, reason: impl Into<String>) -> Self {
        Self::new(signal_type, 0.0, 1.0, reason)
    }

    /// Create a signal indicating extreme risk
    pub fn extreme_risk(signal_type: SignalType, reason: impl Into<String>) -> Self {
        Self::new(signal_type, -1.0, 1.0, reason)
    }

    /// Create a signal indicating high opportunity
    pub fn high_opportunity(signal_type: SignalType, reason: impl Into<String>) -> Self {
        Self::new(signal_type, 1.0, 1.0, reason)
    }

    /// Create a "not available" signal with reduced confidence
    pub fn unavailable(signal_type: SignalType, reason: impl Into<String>) -> Self {
        Self {
            signal_type,
            value: 0.0,
            confidence: 0.0,
            weight: signal_type.default_weight(),
            reason: reason.into(),
            latency: Duration::ZERO,
            cached: false,
        }
    }

    /// Set the latency for this signal
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// Mark this signal as cached
    pub fn with_cached(mut self, cached: bool) -> Self {
        self.cached = cached;
        self
    }

    /// Override the default weight
    pub fn with_weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }

    /// Calculate the effective contribution to scoring
    pub fn effective_contribution(&self) -> f64 {
        self.value * self.weight * self.confidence
    }

    /// Check if this is a risk signal (negative value)
    pub fn is_risk(&self) -> bool {
        self.value < 0.0
    }

    /// Check if this is an opportunity signal (positive value)
    pub fn is_opportunity(&self) -> bool {
        self.value > 0.0
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {:.2} (conf: {:.2}, weight: {:.1}) - {}",
            self.signal_type, self.value, self.confidence, self.weight, self.reason
        )
    }
}

/// Trait for signal providers
///
/// Signal providers compute signals from token/trade data.
/// They can be hot-path (fast, cached) or background (async, RPC-based).
#[async_trait]
pub trait SignalProvider: Send + Sync {
    /// Provider name for logging
    fn name(&self) -> &'static str;

    /// Signal types this provider computes
    fn signal_types(&self) -> &[SignalType];

    /// Is this provider suitable for hot path? (< 10ms expected latency)
    fn is_hot_path(&self) -> bool;

    /// Maximum latency before timeout (ms)
    fn max_latency_ms(&self) -> u64 {
        if self.is_hot_path() {
            50
        } else {
            2000
        }
    }

    /// Compute signals for a new token event
    async fn compute_token_signals(&self, context: &SignalContext) -> Vec<Signal>;

    /// Compute signals for a trade event (optional, for order flow analysis)
    async fn compute_trade_signals(&self, _context: &TradeSignalContext) -> Vec<Signal> {
        Vec::new()
    }

    /// Compute signals for position reassessment (optional)
    async fn compute_position_signals(&self, _context: &PositionSignalContext) -> Vec<Signal> {
        Vec::new()
    }
}

/// Duration serialization in milliseconds for serde
mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_millis().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_creation() {
        let signal = Signal::new(
            SignalType::KnownDeployer,
            -0.9,
            1.0,
            "Known rug deployer",
        );
        assert_eq!(signal.signal_type, SignalType::KnownDeployer);
        assert!(signal.is_risk());
        assert!(!signal.is_opportunity());
    }

    #[test]
    fn test_signal_clamping() {
        let signal = Signal::new(SignalType::NameQuality, 2.5, 1.5, "Over range");
        assert_eq!(signal.value, 1.0);
        assert_eq!(signal.confidence, 1.0);
    }

    #[test]
    fn test_effective_contribution() {
        let signal = Signal::new(SignalType::KnownDeployer, -0.9, 0.8, "Test");
        let contribution = signal.effective_contribution();
        // -0.9 * 2.0 (default weight) * 0.8 = -1.44
        assert!((contribution - (-1.44)).abs() < 0.01);
    }

    #[test]
    fn test_signal_type_hot_path() {
        assert!(SignalType::KnownDeployer.is_hot_path());
        assert!(SignalType::NameQuality.is_hot_path());
        assert!(!SignalType::WalletHistory.is_hot_path());
        assert!(!SignalType::ConcentrationRisk.is_hot_path());
    }
}
