//! Regime Classifier
//!
//! Different playbooks for different token types.
//! Classifies tokens into: Organic Pump, Sniper Flip, Wash Trade, Deployer Bleed.

use serde::{Deserialize, Serialize};

use super::delta_tracker::DeltaMetrics;
use super::types::TokenRegime;

/// Order flow analysis for regime classification
#[derive(Debug, Clone, Default)]
pub struct OrderFlowAnalysis {
    pub organic_score: f64,
    pub wash_trading_score: f64,
    pub buy_sell_ratio: f64,
    pub early_sell_pressure: f64,
    pub burst_detected: bool,
    pub burst_intensity: f64,
}

/// Token distribution analysis
#[derive(Debug, Clone, Default)]
pub struct TokenDistribution {
    pub top_holder_pct: f64,
    pub top_10_holders_pct: f64,
    pub sniper_holdings_pct: f64,
    pub deployer_holdings_pct: f64,
    pub holder_count: u32,
    pub gini_coefficient: f64,
}

/// Creator behavior analysis
#[derive(Debug, Clone, Default)]
pub struct CreatorBehavior {
    pub selling_consistently: bool,
    pub total_sold_pct: f64,
    pub avg_sell_interval_secs: u64,
    pub sell_count: u32,
}

/// Regime classification result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeClassification {
    pub regime: TokenRegime,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub should_enter: bool,
    pub size_multiplier: f64,
}

/// Regime Classifier
pub struct RegimeClassifier {
    /// Wash trading threshold
    wash_threshold: f64,
    /// Sniper holdings threshold
    sniper_threshold: f64,
    /// Deployer bleed threshold
    deployer_threshold: f64,
    /// Minimum organic score for entry
    min_organic_score: f64,
}

impl RegimeClassifier {
    /// Create a new regime classifier
    pub fn new() -> Self {
        Self {
            wash_threshold: 0.6,
            sniper_threshold: 0.4,
            deployer_threshold: 0.3,
            min_organic_score: 0.5,
        }
    }

    /// Classify the token regime
    pub fn classify(
        &self,
        order_flow: &OrderFlowAnalysis,
        distribution: &TokenDistribution,
        creator_behavior: &CreatorBehavior,
        delta: &DeltaMetrics,
    ) -> RegimeClassification {
        let mut reasons = vec![];

        // Check for Wash Trade (highest priority bad regime)
        if order_flow.wash_trading_score > self.wash_threshold {
            reasons.push(format!(
                "Wash trading score {:.0}% exceeds threshold",
                order_flow.wash_trading_score * 100.0
            ));
            return RegimeClassification {
                regime: TokenRegime::WashTrade {
                    wash_pct: order_flow.wash_trading_score,
                    real_volume_sol: order_flow.organic_score, // Rough proxy
                },
                confidence: order_flow.wash_trading_score,
                reasons,
                should_enter: false,
                size_multiplier: 0.0,
            };
        }

        // Check for Deployer Bleed
        if creator_behavior.selling_consistently
            && distribution.deployer_holdings_pct > self.deployer_threshold
        {
            reasons.push(format!(
                "Creator selling consistently ({} sells)",
                creator_behavior.sell_count
            ));
            reasons.push(format!(
                "Creator still holds {:.0}%",
                distribution.deployer_holdings_pct * 100.0
            ));
            return RegimeClassification {
                regime: TokenRegime::DeployerBleed {
                    deployer_holdings_pct: distribution.deployer_holdings_pct * 100.0,
                    avg_sell_interval_secs: creator_behavior.avg_sell_interval_secs,
                },
                confidence: 0.8,
                reasons,
                should_enter: false,
                size_multiplier: 0.0,
            };
        }

        // Check for Sniper Flip
        if distribution.sniper_holdings_pct > self.sniper_threshold
            && order_flow.early_sell_pressure > 0.3
        {
            reasons.push(format!(
                "Snipers hold {:.0}%",
                distribution.sniper_holdings_pct * 100.0
            ));
            reasons.push(format!(
                "Early sell pressure {:.0}%",
                order_flow.early_sell_pressure * 100.0
            ));

            let expected_dump_secs = if order_flow.early_sell_pressure > 0.5 {
                30
            } else {
                60
            };

            return RegimeClassification {
                regime: TokenRegime::SniperFlip {
                    sniper_count: (distribution.sniper_holdings_pct
                        * distribution.holder_count as f64)
                        as u32,
                    expected_dump_in_secs: expected_dump_secs,
                },
                confidence: 0.7,
                reasons,
                should_enter: true,   // Can still scalp
                size_multiplier: 0.3, // Small size
            };
        }

        // Check for Organic Pump
        if order_flow.organic_score > self.min_organic_score {
            reasons.push(format!(
                "Organic score {:.0}%",
                order_flow.organic_score * 100.0
            ));

            // Determine confidence based on multiple factors
            let mut confidence = order_flow.organic_score;

            // Boost confidence if distribution is healthy
            if distribution.gini_coefficient < 0.7 {
                confidence += 0.1;
                reasons.push("Healthy distribution".to_string());
            }

            // Boost if buy ratio is good
            if order_flow.buy_sell_ratio > 0.6 {
                confidence += 0.1;
                reasons.push(format!(
                    "Buy ratio {:.0}%",
                    order_flow.buy_sell_ratio * 100.0
                ));
            }

            // Check momentum
            if delta.overall_trend.is_positive() {
                confidence += 0.05;
                reasons.push("Positive momentum".to_string());
            }

            confidence = confidence.min(0.95);

            let expected_duration = if confidence > 0.8 {
                120
            } else if confidence > 0.7 {
                90
            } else {
                60
            };

            let size_mult = if confidence > 0.8 { 1.5 } else { 1.0 };

            return RegimeClassification {
                regime: TokenRegime::OrganicPump {
                    confidence,
                    expected_duration_secs: expected_duration,
                },
                confidence,
                reasons,
                should_enter: true,
                size_multiplier: size_mult,
            };
        }

        // Default: Unknown regime
        reasons.push("Insufficient data for classification".to_string());
        RegimeClassification {
            regime: TokenRegime::Unknown {
                data_completeness: order_flow.organic_score,
            },
            confidence: order_flow.organic_score,
            reasons,
            should_enter: order_flow.organic_score > 0.3,
            size_multiplier: 0.5,
        }
    }

    /// Quick classification from limited data
    pub fn quick_classify(&self, organic_score: f64, wash_score: f64) -> TokenRegime {
        if wash_score > self.wash_threshold {
            TokenRegime::WashTrade {
                wash_pct: wash_score,
                real_volume_sol: 0.0,
            }
        } else if organic_score > self.min_organic_score {
            TokenRegime::OrganicPump {
                confidence: organic_score,
                expected_duration_secs: 60,
            }
        } else {
            TokenRegime::Unknown {
                data_completeness: organic_score,
            }
        }
    }
}

impl Default for RegimeClassifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::Trend;
    use super::*;

    #[test]
    fn test_wash_trade_detection() {
        let classifier = RegimeClassifier::new();

        let order_flow = OrderFlowAnalysis {
            wash_trading_score: 0.8,
            organic_score: 0.2,
            ..Default::default()
        };

        let result = classifier.classify(
            &order_flow,
            &TokenDistribution::default(),
            &CreatorBehavior::default(),
            &DeltaMetrics::default(),
        );

        assert!(matches!(result.regime, TokenRegime::WashTrade { .. }));
        assert!(!result.should_enter);
        assert_eq!(result.size_multiplier, 0.0);
    }

    #[test]
    fn test_deployer_bleed_detection() {
        let classifier = RegimeClassifier::new();

        let distribution = TokenDistribution {
            deployer_holdings_pct: 0.5,
            ..Default::default()
        };

        let creator = CreatorBehavior {
            selling_consistently: true,
            sell_count: 5,
            ..Default::default()
        };

        let result = classifier.classify(
            &OrderFlowAnalysis::default(),
            &distribution,
            &creator,
            &DeltaMetrics::default(),
        );

        assert!(matches!(result.regime, TokenRegime::DeployerBleed { .. }));
        assert!(!result.should_enter);
    }

    #[test]
    fn test_sniper_flip_detection() {
        let classifier = RegimeClassifier::new();

        let order_flow = OrderFlowAnalysis {
            organic_score: 0.4,
            early_sell_pressure: 0.4,
            ..Default::default()
        };

        let distribution = TokenDistribution {
            sniper_holdings_pct: 0.5,
            holder_count: 100,
            ..Default::default()
        };

        let result = classifier.classify(
            &order_flow,
            &distribution,
            &CreatorBehavior::default(),
            &DeltaMetrics::default(),
        );

        assert!(matches!(result.regime, TokenRegime::SniperFlip { .. }));
        assert!(result.should_enter); // Can scalp
        assert_eq!(result.size_multiplier, 0.3);
    }

    #[test]
    fn test_organic_pump_detection() {
        let classifier = RegimeClassifier::new();

        let order_flow = OrderFlowAnalysis {
            organic_score: 0.8,
            buy_sell_ratio: 0.7,
            wash_trading_score: 0.1,
            ..Default::default()
        };

        let distribution = TokenDistribution {
            gini_coefficient: 0.5,
            ..Default::default()
        };

        let delta = DeltaMetrics {
            overall_trend: Trend::Improving,
            ..Default::default()
        };

        let result = classifier.classify(
            &order_flow,
            &distribution,
            &CreatorBehavior::default(),
            &delta,
        );

        assert!(matches!(result.regime, TokenRegime::OrganicPump { .. }));
        assert!(result.should_enter);
        assert!(result.size_multiplier >= 1.0);
    }

    #[test]
    fn test_unknown_regime() {
        let classifier = RegimeClassifier::new();

        let order_flow = OrderFlowAnalysis {
            organic_score: 0.3,
            ..Default::default()
        };

        let result = classifier.classify(
            &order_flow,
            &TokenDistribution::default(),
            &CreatorBehavior::default(),
            &DeltaMetrics::default(),
        );

        assert!(matches!(result.regime, TokenRegime::Unknown { .. }));
    }

    #[test]
    fn test_quick_classify() {
        let classifier = RegimeClassifier::new();

        // Wash trade
        let regime = classifier.quick_classify(0.2, 0.8);
        assert!(matches!(regime, TokenRegime::WashTrade { .. }));

        // Organic
        let regime = classifier.quick_classify(0.8, 0.1);
        assert!(matches!(regime, TokenRegime::OrganicPump { .. }));

        // Unknown
        let regime = classifier.quick_classify(0.3, 0.3);
        assert!(matches!(regime, TokenRegime::Unknown { .. }));
    }
}
