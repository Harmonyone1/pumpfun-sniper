//! Scoring engine for aggregating signals into trading decisions
//!
//! The scoring engine takes multiple signals and produces a single score
//! with a recommendation (Buy, Skip, Avoid, etc.) and position sizing.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::filter::signals::{Signal, SignalCategory, SignalType};

/// Final scoring result with recommendation
#[derive(Debug, Clone, Serialize)]
pub struct ScoringResult {
    /// Overall score (-1.0 = extreme risk, +1.0 = extreme opportunity)
    pub score: f64,
    /// Risk score (0.0 to 1.0, higher = riskier)
    pub risk_score: f64,
    /// Opportunity score (0.0 to 1.0, higher = better opportunity)
    pub opportunity_score: f64,
    /// Confidence in overall score (0.0 to 1.0)
    pub confidence: f64,
    /// Individual signals that contributed
    pub signals: Vec<Signal>,
    /// Decision recommendation
    pub recommendation: Recommendation,
    /// Suggested position size multiplier (0.0 to 2.0)
    /// 1.0 = normal position, 0.5 = half position, 2.0 = double position
    pub position_size_multiplier: f64,
    /// Human-readable summary
    pub summary: String,
}

impl Default for ScoringResult {
    fn default() -> Self {
        Self {
            score: 0.0,
            risk_score: 0.5,
            opportunity_score: 0.0,
            confidence: 0.0,
            signals: Vec::new(),
            recommendation: Recommendation::Observe,  // Default: watch, don't trade
            position_size_multiplier: 0.0,
            summary: "No signals available".to_string(),
        }
    }
}

/// System readiness state for trading decisions
///
/// Trading is only allowed when the system has sufficient data.
/// This implements the "Readiness Gate" concept.
#[derive(Debug, Clone, Serialize)]
pub struct ReadinessState {
    /// Data completeness score (0.0 to 1.0)
    pub data_completeness: f64,
    /// Number of enriched data components (metadata, wallet history, order flow, etc.)
    pub enriched_components: u32,
    /// Time since token launch in seconds
    pub time_since_launch_secs: u64,
    /// Whether the system meets minimum readiness for ANY trading
    pub is_ready_for_trading: bool,
    /// Whether the system meets readiness for full positions (not just probe)
    pub is_ready_for_full_position: bool,
    /// Reason if not ready
    pub readiness_reason: Option<String>,
}

impl Default for ReadinessState {
    fn default() -> Self {
        Self {
            data_completeness: 0.0,
            enriched_components: 0,
            time_since_launch_secs: 0,
            is_ready_for_trading: false,
            is_ready_for_full_position: false,
            readiness_reason: Some("No data available".to_string()),
        }
    }
}

impl ReadinessState {
    /// Create a readiness state from available data
    pub fn evaluate(
        thresholds: &ScoringThresholds,
        data_completeness: f64,
        enriched_components: u32,
        time_since_launch_secs: u64,
    ) -> Self {
        let mut reasons = Vec::new();

        if data_completeness < thresholds.min_data_completeness {
            reasons.push(format!(
                "data completeness {:.0}% < {:.0}%",
                data_completeness * 100.0,
                thresholds.min_data_completeness * 100.0
            ));
        }

        if enriched_components < thresholds.min_enriched_components {
            reasons.push(format!(
                "{} enriched components < {}",
                enriched_components, thresholds.min_enriched_components
            ));
        }

        if time_since_launch_secs < thresholds.min_time_since_launch_secs {
            reasons.push(format!(
                "{}s since launch < {}s",
                time_since_launch_secs, thresholds.min_time_since_launch_secs
            ));
        }

        let is_ready_for_trading = reasons.is_empty();

        // Full position requires higher bar
        let is_ready_for_full_position = is_ready_for_trading
            && data_completeness >= 0.7
            && enriched_components >= 3;

        let readiness_reason = if reasons.is_empty() {
            None
        } else {
            Some(reasons.join(", "))
        };

        Self {
            data_completeness,
            enriched_components,
            time_since_launch_secs,
            is_ready_for_trading,
            is_ready_for_full_position,
            readiness_reason,
        }
    }
}

impl ScoringResult {
    /// Create a fail-closed result (used when protocol errors occur)
    pub fn fail_closed(reason: &str) -> Self {
        Self {
            score: -1.0,
            risk_score: 1.0,
            opportunity_score: 0.0,
            confidence: 1.0,
            signals: Vec::new(),
            recommendation: Recommendation::Avoid,
            position_size_multiplier: 0.0,
            summary: format!("FAIL-CLOSED: {}", reason),
        }
    }

    /// Check if this result allows trading (full position)
    /// Only StrongBuy and Opportunity allow full positions
    pub fn should_buy(&self) -> bool {
        self.recommendation.is_full_position()
    }

    /// Check if this result allows any trading action (including probe)
    pub fn allows_any_trading(&self) -> bool {
        self.recommendation.allows_trading()
    }

    /// Check if this is a probe trade (learning mode)
    pub fn is_probe(&self) -> bool {
        matches!(self.recommendation, Recommendation::Probe)
    }

    /// Get signal by type
    pub fn get_signal(&self, signal_type: SignalType) -> Option<&Signal> {
        self.signals.iter().find(|s| s.signal_type == signal_type)
    }

    /// Get all risk signals
    pub fn risk_signals(&self) -> Vec<&Signal> {
        self.signals.iter().filter(|s| s.is_risk()).collect()
    }

    /// Get all opportunity signals
    pub fn opportunity_signals(&self) -> Vec<&Signal> {
        self.signals.iter().filter(|s| s.is_opportunity()).collect()
    }
}

/// Trading recommendation based on confidence regime model
///
/// When information is weak, the system watches — not trades.
/// When confidence is earned, capital is deployed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recommendation {
    /// Strong buy signal - high confidence (score >= 0.65)
    StrongBuy,
    /// Opportunity - standard buy (score >= 0.35)
    Opportunity,
    /// Probe mode - micro-position for learning (score 0.15-0.35)
    /// Uses 5% position size, quick scalp exit
    Probe,
    /// Observe only - watch but don't trade (score < 0.15)
    /// This replaces "Caution" - when uncertain, WATCH not trade
    Observe,
    /// High risk - definitely avoid
    Avoid,
}

impl Recommendation {
    /// Get a human-readable description
    pub fn description(&self) -> &'static str {
        match self {
            Recommendation::StrongBuy => "Strong positive signals - buy with confidence",
            Recommendation::Opportunity => "Good opportunity - standard buy",
            Recommendation::Probe => "Probe mode - micro-position for learning only",
            Recommendation::Observe => "Observe only - watch, don't trade",
            Recommendation::Avoid => "High risk detected - avoid",
        }
    }

    /// Check if this recommendation allows any trading action
    pub fn allows_trading(&self) -> bool {
        matches!(self, Recommendation::StrongBuy | Recommendation::Opportunity | Recommendation::Probe)
    }

    /// Check if this is a full-conviction trade (not probe)
    pub fn is_full_position(&self) -> bool {
        matches!(self, Recommendation::StrongBuy | Recommendation::Opportunity)
    }

    /// Get position size multiplier for this recommendation
    pub fn position_multiplier(&self) -> f64 {
        match self {
            Recommendation::StrongBuy => 1.5,
            Recommendation::Opportunity => 1.0,
            Recommendation::Probe => 0.05,  // 5% probe position
            Recommendation::Observe => 0.0,
            Recommendation::Avoid => 0.0,
        }
    }
}

/// Score thresholds for recommendations
///
/// New confidence regime model:
/// - StrongBuy: score >= 0.65 (high conviction)
/// - Opportunity: score >= 0.35 (standard position)
/// - Probe: score >= 0.15 (micro-position for learning)
/// - Observe: score < 0.15 (watch only, no trade)
/// - Avoid: score < -0.3 (high risk)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringThresholds {
    /// Score above this = StrongBuy (high conviction)
    pub strong_buy: f64,
    /// Score above this = Opportunity (standard position)
    pub opportunity: f64,
    /// Score above this = Probe (micro-position for learning)
    pub probe: f64,
    /// Score below this = Avoid (high risk)
    pub avoid: f64,
    /// Minimum confidence to act on score
    pub min_confidence: f64,
    /// Minimum data completeness for any trading action (0.0 to 1.0)
    pub min_data_completeness: f64,
    /// Minimum enriched components for trading (e.g., 3 = metadata + wallet + order_flow)
    pub min_enriched_components: u32,
    /// Minimum time since launch before trading (seconds)
    pub min_time_since_launch_secs: u64,
}

impl Default for ScoringThresholds {
    fn default() -> Self {
        Self {
            strong_buy: 0.40,      // Lowered from 0.65 - more aggressive
            opportunity: 0.10,     // Lowered from 0.35 - allow more opportunities
            probe: -0.20,          // Lowered from 0.15 - probe even negative scores
            avoid: -0.50,          // Lowered from -0.3 - only avoid very bad tokens
            min_confidence: 0.20,  // Lowered from 0.3 - trade with less certainty
            min_data_completeness: 0.3,     // Lowered from 0.5 - trade with less data
            min_enriched_components: 1,       // Lowered from 2 - need less enrichment
            min_time_since_launch_secs: 5,    // Lowered from 15s - faster entry
        }
    }
}

/// The main scoring engine
pub struct ScoringEngine {
    /// Signal weights by type (overrides defaults)
    weights: HashMap<SignalType, f64>,
    /// Score thresholds for recommendations
    thresholds: ScoringThresholds,
}

impl ScoringEngine {
    /// Create a new scoring engine with default weights
    pub fn new() -> Self {
        Self {
            weights: HashMap::new(),
            thresholds: ScoringThresholds::default(),
        }
    }

    /// Create with custom thresholds
    pub fn with_thresholds(thresholds: ScoringThresholds) -> Self {
        Self {
            weights: HashMap::new(),
            thresholds,
        }
    }

    /// Set custom weight for a signal type
    pub fn set_weight(&mut self, signal_type: SignalType, weight: f64) {
        self.weights.insert(signal_type, weight);
    }

    /// Set weights from a map
    pub fn set_weights(&mut self, weights: HashMap<SignalType, f64>) {
        self.weights = weights;
    }

    /// Get effective weight for a signal type
    fn get_weight(&self, signal_type: SignalType) -> f64 {
        self.weights
            .get(&signal_type)
            .copied()
            .unwrap_or_else(|| signal_type.default_weight())
    }

    /// Aggregate signals into a scoring result
    pub fn score(&self, signals: Vec<Signal>) -> ScoringResult {
        if signals.is_empty() {
            return ScoringResult::default();
        }

        // Apply configured weights and compute effective contributions
        let weighted_signals: Vec<Signal> = signals
            .into_iter()
            .map(|mut s| {
                s.weight = self.get_weight(s.signal_type);
                s
            })
            .collect();

        // Calculate weighted average with confidence
        let mut weighted_sum = 0.0;
        let mut weight_sum = 0.0;
        let mut confidence_sum = 0.0;

        let mut risk_signals = Vec::new();
        let mut opportunity_signals = Vec::new();

        for signal in &weighted_signals {
            let effective_weight = signal.weight * signal.confidence;

            weighted_sum += signal.value * effective_weight;
            weight_sum += effective_weight;
            confidence_sum += signal.confidence;

            if signal.is_risk() {
                risk_signals.push(signal);
            } else if signal.is_opportunity() {
                opportunity_signals.push(signal);
            }
        }

        let score = if weight_sum > 0.0 {
            weighted_sum / weight_sum
        } else {
            0.0
        };

        let confidence = confidence_sum / weighted_signals.len() as f64;

        // Calculate separate risk and opportunity scores
        let risk_score = risk_signals
            .iter()
            .map(|s| s.value.abs() * s.confidence * s.weight)
            .sum::<f64>()
            .min(1.0);

        let opportunity_score = opportunity_signals
            .iter()
            .map(|s| s.value * s.confidence * s.weight)
            .sum::<f64>()
            .min(1.0)
            .max(0.0);

        // Determine recommendation
        let recommendation = self.score_to_recommendation(score, confidence);

        // Calculate position size multiplier
        let position_size_multiplier =
            self.calculate_position_multiplier(score, confidence, risk_score, &recommendation);

        // Generate summary
        let summary = self.generate_summary(&weighted_signals, score, &recommendation);

        ScoringResult {
            score,
            risk_score,
            opportunity_score,
            confidence,
            signals: weighted_signals,
            recommendation,
            position_size_multiplier,
            summary,
        }
    }

    /// Convert score to recommendation using confidence regime model
    ///
    /// When information is weak, the system watches — not trades.
    /// When confidence is earned, capital is deployed.
    fn score_to_recommendation(&self, score: f64, confidence: f64) -> Recommendation {
        // First check for avoid conditions
        if score < self.thresholds.avoid {
            return Recommendation::Avoid;
        }

        // Low confidence = Observe (watch, don't trade)
        // This is the key change: uncertainty -> observation, not reduced position
        if confidence < self.thresholds.min_confidence {
            return Recommendation::Observe;
        }

        // High conviction trading decisions
        if score >= self.thresholds.strong_buy {
            Recommendation::StrongBuy
        } else if score >= self.thresholds.opportunity {
            Recommendation::Opportunity
        } else if score >= self.thresholds.probe {
            // Probe mode: micro-position for learning
            // Only when we have SOME signal but not enough for full conviction
            Recommendation::Probe
        } else {
            // Score too low for even probe mode
            Recommendation::Observe
        }
    }

    /// Calculate position size multiplier based on recommendation
    ///
    /// Uses the recommendation's built-in position multiplier,
    /// adjusted by risk and confidence factors.
    fn calculate_position_multiplier(
        &self,
        score: f64,
        confidence: f64,
        risk_score: f64,
        recommendation: &Recommendation,
    ) -> f64 {
        // No position for Observe/Avoid
        if !recommendation.allows_trading() {
            return 0.0;
        }

        // Get base multiplier from recommendation
        let base = recommendation.position_multiplier();

        // For Probe mode, use fixed small size (5%)
        if matches!(recommendation, Recommendation::Probe) {
            // Probe is intentionally small and not adjusted by other factors
            // It's for learning, not profit
            return base; // 0.05
        }

        // For full positions, apply adjustments
        // Confidence factor: scale with confidence (0.7 to 1.0)
        let confidence_factor = 0.7 + (confidence * 0.3);

        // Risk factor: reduce for high risk (0.5 to 1.0)
        let risk_factor = 1.0 - (risk_score * 0.5);

        // Score factor: scale linearly within range (0.8 to 1.2)
        let score_factor = 0.8 + (score.clamp(0.0, 1.0) * 0.4);

        (base * confidence_factor * risk_factor * score_factor).clamp(0.1, 2.0)
    }

    /// Generate human-readable summary
    fn generate_summary(
        &self,
        signals: &[Signal],
        score: f64,
        recommendation: &Recommendation,
    ) -> String {
        let risk_count = signals.iter().filter(|s| s.is_risk()).count();
        let opportunity_count = signals.iter().filter(|s| s.is_opportunity()).count();

        // Find top risk and opportunity signals
        let top_risk = signals
            .iter()
            .filter(|s| s.is_risk())
            .max_by(|a, b| {
                a.effective_contribution()
                    .abs()
                    .partial_cmp(&b.effective_contribution().abs())
                    .unwrap()
            });

        let top_opportunity = signals
            .iter()
            .filter(|s| s.is_opportunity())
            .max_by(|a, b| {
                a.effective_contribution()
                    .partial_cmp(&b.effective_contribution())
                    .unwrap()
            });

        let mut parts = Vec::new();

        parts.push(format!(
            "Score: {:.2} -> {:?}",
            score, recommendation
        ));
        parts.push(format!(
            "{} signals ({} risk, {} opportunity)",
            signals.len(),
            risk_count,
            opportunity_count
        ));

        if let Some(risk) = top_risk {
            parts.push(format!("Top risk: {} ({:.2})", risk.signal_type, risk.value));
        }

        if let Some(opp) = top_opportunity {
            parts.push(format!(
                "Top opportunity: {} ({:.2})",
                opp.signal_type, opp.value
            ));
        }

        parts.join(" | ")
    }

    /// Score by category (for analysis)
    pub fn score_by_category(&self, signals: &[Signal]) -> HashMap<SignalCategory, f64> {
        let mut category_scores: HashMap<SignalCategory, (f64, f64)> = HashMap::new();

        for signal in signals {
            let category = signal.signal_type.category();
            let effective_weight = signal.weight * signal.confidence;

            let entry = category_scores.entry(category).or_insert((0.0, 0.0));
            entry.0 += signal.value * effective_weight;
            entry.1 += effective_weight;
        }

        category_scores
            .into_iter()
            .map(|(cat, (sum, weight))| {
                let score = if weight > 0.0 { sum / weight } else { 0.0 };
                (cat, score)
            })
            .collect()
    }
}

impl Default for ScoringEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::signals::Signal;

    #[test]
    fn test_scoring_empty() {
        let engine = ScoringEngine::new();
        let result = engine.score(vec![]);
        assert_eq!(result.score, 0.0);
        // Low confidence = Observe (don't trade)
        assert_eq!(result.recommendation, Recommendation::Observe);
    }

    #[test]
    fn test_scoring_high_score_strong_buy() {
        let engine = ScoringEngine::new();
        // High value signals with high confidence = StrongBuy
        let signals = vec![
            Signal::new(SignalType::NameQuality, 0.8, 1.0, "Great name"),
            Signal::new(SignalType::LiquiditySeeding, 0.7, 0.9, "Excellent liquidity"),
        ];
        let result = engine.score(signals);
        assert!(result.score >= 0.65);
        assert!(result.should_buy());
        assert_eq!(result.recommendation, Recommendation::StrongBuy);
    }

    #[test]
    fn test_scoring_medium_score_opportunity() {
        let engine = ScoringEngine::new();
        // Medium value signals = Opportunity
        let signals = vec![
            Signal::new(SignalType::NameQuality, 0.5, 1.0, "Good name"),
            Signal::new(SignalType::LiquiditySeeding, 0.4, 0.8, "Normal liquidity"),
        ];
        let result = engine.score(signals);
        assert!(result.score >= 0.35 && result.score < 0.65);
        assert!(result.should_buy());
        assert_eq!(result.recommendation, Recommendation::Opportunity);
    }

    #[test]
    fn test_scoring_low_score_probe() {
        let engine = ScoringEngine::new();
        // Lower value signals = Probe mode
        let signals = vec![
            Signal::new(SignalType::NameQuality, 0.2, 1.0, "Okay name"),
            Signal::new(SignalType::LiquiditySeeding, 0.15, 0.8, "Basic liquidity"),
        ];
        let result = engine.score(signals);
        // Score should be in probe range (0.15-0.35)
        if result.score >= 0.15 && result.score < 0.35 {
            assert_eq!(result.recommendation, Recommendation::Probe);
            assert!(result.is_probe());
            assert!(!result.should_buy()); // Probe is NOT a full buy
            assert!(result.allows_any_trading()); // But it allows trading
        }
    }

    #[test]
    fn test_scoring_negative() {
        let engine = ScoringEngine::new();
        let signals = vec![
            Signal::new(SignalType::KnownDeployer, -0.9, 1.0, "Known rug deployer"),
            Signal::new(SignalType::WashTrading, -0.7, 0.8, "Wash trading detected"),
        ];
        let result = engine.score(signals);
        assert!(result.score < 0.0);
        assert!(!result.should_buy());
        assert_eq!(result.recommendation, Recommendation::Avoid);
    }

    #[test]
    fn test_scoring_low_confidence_observe() {
        let engine = ScoringEngine::new();
        // Good score but LOW confidence = Observe (not trade)
        let signals = vec![
            Signal::new(SignalType::NameQuality, 0.5, 0.1, "Good name but uncertain"),
        ];
        let result = engine.score(signals);
        // Low confidence should result in Observe
        assert_eq!(result.recommendation, Recommendation::Observe);
        assert!(!result.allows_any_trading());
    }

    #[test]
    fn test_position_multiplier_by_recommendation() {
        // StrongBuy should have highest multiplier
        assert_eq!(Recommendation::StrongBuy.position_multiplier(), 1.5);
        // Opportunity = normal
        assert_eq!(Recommendation::Opportunity.position_multiplier(), 1.0);
        // Probe = small (5%)
        assert_eq!(Recommendation::Probe.position_multiplier(), 0.05);
        // Observe = no trading
        assert_eq!(Recommendation::Observe.position_multiplier(), 0.0);
        // Avoid = no trading
        assert_eq!(Recommendation::Avoid.position_multiplier(), 0.0);
    }

    #[test]
    fn test_fail_closed() {
        let result = ScoringResult::fail_closed("Protocol decode error");
        assert_eq!(result.score, -1.0);
        assert_eq!(result.recommendation, Recommendation::Avoid);
        assert_eq!(result.position_size_multiplier, 0.0);
    }

    #[test]
    fn test_readiness_state() {
        let thresholds = ScoringThresholds::default();

        // Not ready - missing data
        let state = ReadinessState::evaluate(&thresholds, 0.3, 1, 5);
        assert!(!state.is_ready_for_trading);
        assert!(state.readiness_reason.is_some());

        // Ready for trading
        let state = ReadinessState::evaluate(&thresholds, 0.6, 3, 20);
        assert!(state.is_ready_for_trading);
        assert!(state.readiness_reason.is_none());

        // Ready for full position (higher bar)
        let state = ReadinessState::evaluate(&thresholds, 0.8, 4, 30);
        assert!(state.is_ready_for_trading);
        assert!(state.is_ready_for_full_position);
    }

    #[test]
    fn test_custom_weights() {
        let mut engine = ScoringEngine::new();
        engine.set_weight(SignalType::NameQuality, 5.0); // Much higher weight

        let signals = vec![
            Signal::new(SignalType::NameQuality, 0.5, 1.0, "Good name"),
            Signal::new(SignalType::KnownDeployer, -0.3, 1.0, "Unknown"),
        ];
        let result = engine.score(signals);

        // Name quality should dominate with 5.0 weight
        assert!(result.score > 0.0);
    }
}
