//! Rug Prediction
//!
//! Early warning system for detecting potential rug pulls.
//! Monitors creator behavior, liquidity changes, and insider activity.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Rug predictor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RugPredictorConfig {
    pub enabled: bool,
    pub creator_sell_warning_pct: f64,
    pub liquidity_drain_warning_pct: f64,
    pub insider_sell_threshold: u32,
    pub wash_volume_threshold_pct: f64,
}

impl Default for RugPredictorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            creator_sell_warning_pct: 10.0,
            liquidity_drain_warning_pct: 20.0,
            insider_sell_threshold: 3,
            wash_volume_threshold_pct: 50.0,
        }
    }
}

/// Rug warning signal types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RugWarningSignal {
    CreatorStartedSelling {
        pct_sold: f64,
        sell_count: u32,
    },
    LiquidityDraining {
        drop_pct: f64,
        rate_per_min: f64,
    },
    InsiderExodus {
        top_holder_sells: u32,
        total_sold_pct: f64,
    },
    VolumeWithoutPriceRise {
        volume_sol: f64,
        price_change_pct: f64,
    },
    SuddenMetadataChange {
        field_changed: String,
    },
    ConcentratedHoldings {
        top_holder_pct: f64,
    },
    RapidSelloff {
        sell_count: u32,
        within_secs: u64,
    },
}

impl RugWarningSignal {
    /// Get severity weight for this signal
    pub fn severity(&self) -> f64 {
        match self {
            RugWarningSignal::CreatorStartedSelling { pct_sold, .. } => {
                if *pct_sold > 50.0 {
                    0.9
                } else if *pct_sold > 20.0 {
                    0.7
                } else {
                    0.4
                }
            }
            RugWarningSignal::LiquidityDraining { drop_pct, .. } => {
                if *drop_pct > 50.0 {
                    0.95
                } else if *drop_pct > 30.0 {
                    0.8
                } else {
                    0.5
                }
            }
            RugWarningSignal::InsiderExodus { top_holder_sells, .. } => {
                if *top_holder_sells > 5 {
                    0.85
                } else if *top_holder_sells > 3 {
                    0.6
                } else {
                    0.3
                }
            }
            RugWarningSignal::VolumeWithoutPriceRise { .. } => 0.4,
            RugWarningSignal::SuddenMetadataChange { .. } => 0.3,
            RugWarningSignal::ConcentratedHoldings { top_holder_pct } => {
                if *top_holder_pct > 80.0 {
                    0.7
                } else if *top_holder_pct > 50.0 {
                    0.4
                } else {
                    0.2
                }
            }
            RugWarningSignal::RapidSelloff { sell_count, .. } => {
                if *sell_count > 20 {
                    0.9
                } else if *sell_count > 10 {
                    0.6
                } else {
                    0.3
                }
            }
        }
    }
}

/// Rug prediction result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RugPrediction {
    pub mint: String,
    pub probability: f64,
    pub warnings: Vec<RugWarningSignal>,
    pub recommendation: String,
    pub urgency: RugUrgency,
}

/// Urgency level for rug prediction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RugUrgency {
    Low,       // Monitor
    Medium,    // Consider reducing position
    High,      // Exit recommended
    Critical,  // Exit immediately
}

impl RugUrgency {
    pub fn from_probability(prob: f64) -> Self {
        if prob > 0.8 {
            RugUrgency::Critical
        } else if prob > 0.6 {
            RugUrgency::High
        } else if prob > 0.4 {
            RugUrgency::Medium
        } else {
            RugUrgency::Low
        }
    }
}

/// Context for rug prediction
#[derive(Debug, Clone, Default)]
pub struct RugPredictionContext {
    pub mint: String,
    pub creator_sold_pct: f64,
    pub creator_sell_count: u32,
    pub liquidity_sol: f64,
    pub initial_liquidity_sol: f64,
    pub top_holder_pct: f64,
    pub top_holder_sells: u32,
    pub total_insider_sold_pct: f64,
    pub recent_volume_sol: f64,
    pub price_change_pct: f64,
    pub metadata_changed: bool,
    pub metadata_field: Option<String>,
    pub recent_sell_count: u32,
    pub recent_sell_window_secs: u64,
}

/// Rug predictor
pub struct RugPredictor {
    config: RugPredictorConfig,
    token_history: HashMap<String, TokenHistory>,
}

/// Historical data for a token
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct TokenHistory {
    initial_liquidity: f64,
    peak_liquidity: f64,
    creator_initial_holdings: f64,
}

impl RugPredictor {
    /// Create a new rug predictor
    pub fn new(config: RugPredictorConfig) -> Self {
        Self {
            config,
            token_history: HashMap::new(),
        }
    }

    /// Initialize tracking for a new token
    pub fn track_token(&mut self, mint: &str, initial_liquidity: f64, creator_holdings: f64) {
        self.token_history.insert(
            mint.to_string(),
            TokenHistory {
                initial_liquidity,
                peak_liquidity: initial_liquidity,
                creator_initial_holdings: creator_holdings,
            },
        );
    }

    /// Update peak liquidity
    pub fn update_liquidity(&mut self, mint: &str, liquidity: f64) {
        if let Some(history) = self.token_history.get_mut(mint) {
            if liquidity > history.peak_liquidity {
                history.peak_liquidity = liquidity;
            }
        }
    }

    /// Predict rug probability
    pub fn predict(&self, ctx: &RugPredictionContext) -> RugPrediction {
        if !self.config.enabled {
            return RugPrediction {
                mint: ctx.mint.clone(),
                probability: 0.0,
                warnings: vec![],
                recommendation: "Rug prediction disabled".to_string(),
                urgency: RugUrgency::Low,
            };
        }

        let mut warnings = vec![];
        let mut total_severity = 0.0;

        // Check creator selling
        if ctx.creator_sold_pct > self.config.creator_sell_warning_pct {
            let signal = RugWarningSignal::CreatorStartedSelling {
                pct_sold: ctx.creator_sold_pct,
                sell_count: ctx.creator_sell_count,
            };
            total_severity += signal.severity();
            warnings.push(signal);
        }

        // Check liquidity draining
        let history = self.token_history.get(&ctx.mint);
        if let Some(history) = history {
            let liquidity_drop = if history.peak_liquidity > 0.0 {
                ((history.peak_liquidity - ctx.liquidity_sol) / history.peak_liquidity) * 100.0
            } else {
                0.0
            };

            if liquidity_drop > self.config.liquidity_drain_warning_pct {
                let signal = RugWarningSignal::LiquidityDraining {
                    drop_pct: liquidity_drop,
                    rate_per_min: liquidity_drop / 5.0, // Rough estimate
                };
                total_severity += signal.severity();
                warnings.push(signal);
            }
        }

        // Check insider exodus
        if ctx.top_holder_sells > self.config.insider_sell_threshold {
            let signal = RugWarningSignal::InsiderExodus {
                top_holder_sells: ctx.top_holder_sells,
                total_sold_pct: ctx.total_insider_sold_pct,
            };
            total_severity += signal.severity();
            warnings.push(signal);
        }

        // Check wash trading indicator
        if ctx.recent_volume_sol > 0.5 && ctx.price_change_pct.abs() < 5.0 {
            let signal = RugWarningSignal::VolumeWithoutPriceRise {
                volume_sol: ctx.recent_volume_sol,
                price_change_pct: ctx.price_change_pct,
            };
            total_severity += signal.severity();
            warnings.push(signal);
        }

        // Check metadata changes
        if ctx.metadata_changed {
            let signal = RugWarningSignal::SuddenMetadataChange {
                field_changed: ctx.metadata_field.clone().unwrap_or_default(),
            };
            total_severity += signal.severity();
            warnings.push(signal);
        }

        // Check concentrated holdings
        if ctx.top_holder_pct > 50.0 {
            let signal = RugWarningSignal::ConcentratedHoldings {
                top_holder_pct: ctx.top_holder_pct,
            };
            total_severity += signal.severity();
            warnings.push(signal);
        }

        // Check rapid selloff
        if ctx.recent_sell_count > 10 && ctx.recent_sell_window_secs < 60 {
            let signal = RugWarningSignal::RapidSelloff {
                sell_count: ctx.recent_sell_count,
                within_secs: ctx.recent_sell_window_secs,
            };
            total_severity += signal.severity();
            warnings.push(signal);
        }

        // Calculate probability (normalize by expected typical max)
        // In practice, 2-3 warnings at moderate severity is concerning
        let max_severity = 2.5;
        let probability = (total_severity / max_severity).min(0.99);

        let urgency = RugUrgency::from_probability(probability);

        let recommendation = match urgency {
            RugUrgency::Critical => "EXIT IMMEDIATELY - High rug probability".to_string(),
            RugUrgency::High => "Exit recommended - Multiple warning signals".to_string(),
            RugUrgency::Medium => "Reduce position - Concerning signals detected".to_string(),
            RugUrgency::Low => "Monitor - Minor warning signals".to_string(),
        };

        RugPrediction {
            mint: ctx.mint.clone(),
            probability,
            warnings,
            recommendation,
            urgency,
        }
    }

    /// Quick check for immediate danger
    pub fn is_immediate_danger(&self, ctx: &RugPredictionContext) -> bool {
        // Check for immediate red flags
        ctx.creator_sold_pct > 50.0
            || (ctx.liquidity_sol < ctx.initial_liquidity_sol * 0.3)
            || ctx.recent_sell_count > 20
    }

    /// Clear tracking for a token
    pub fn clear(&mut self, mint: &str) {
        self.token_history.remove(mint);
    }
}

impl Default for RugPredictor {
    fn default() -> Self {
        Self::new(RugPredictorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_creator_selling_warning() {
        let predictor = RugPredictor::default();

        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            creator_sold_pct: 25.0,
            creator_sell_count: 3,
            ..Default::default()
        };

        let prediction = predictor.predict(&ctx);
        assert!(prediction.probability > 0.0);
        assert!(!prediction.warnings.is_empty());
        assert!(prediction
            .warnings
            .iter()
            .any(|w| matches!(w, RugWarningSignal::CreatorStartedSelling { .. })));
    }

    #[test]
    fn test_liquidity_draining_warning() {
        let mut predictor = RugPredictor::default();

        predictor.track_token("test_mint", 10.0, 100.0);
        predictor.update_liquidity("test_mint", 10.0);

        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            liquidity_sol: 3.0, // Dropped 70%
            initial_liquidity_sol: 10.0,
            ..Default::default()
        };

        let prediction = predictor.predict(&ctx);
        assert!(prediction.probability > 0.3);
        assert!(prediction
            .warnings
            .iter()
            .any(|w| matches!(w, RugWarningSignal::LiquidityDraining { .. })));
    }

    #[test]
    fn test_insider_exodus_warning() {
        let predictor = RugPredictor::default();

        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            top_holder_sells: 5,
            total_insider_sold_pct: 30.0,
            ..Default::default()
        };

        let prediction = predictor.predict(&ctx);
        assert!(prediction
            .warnings
            .iter()
            .any(|w| matches!(w, RugWarningSignal::InsiderExodus { .. })));
    }

    #[test]
    fn test_urgency_levels() {
        assert_eq!(RugUrgency::from_probability(0.1), RugUrgency::Low);
        assert_eq!(RugUrgency::from_probability(0.5), RugUrgency::Medium);
        assert_eq!(RugUrgency::from_probability(0.7), RugUrgency::High);
        assert_eq!(RugUrgency::from_probability(0.9), RugUrgency::Critical);
    }

    #[test]
    fn test_immediate_danger() {
        let predictor = RugPredictor::default();

        // Creator dumped heavily
        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            creator_sold_pct: 60.0,
            ..Default::default()
        };
        assert!(predictor.is_immediate_danger(&ctx));

        // Liquidity collapsed
        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            liquidity_sol: 1.0,
            initial_liquidity_sol: 10.0,
            ..Default::default()
        };
        assert!(predictor.is_immediate_danger(&ctx));

        // Rapid selloff
        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            recent_sell_count: 25,
            ..Default::default()
        };
        assert!(predictor.is_immediate_danger(&ctx));
    }

    #[test]
    fn test_no_warnings_clean_token() {
        let predictor = RugPredictor::default();

        let ctx = RugPredictionContext {
            mint: "test_mint".to_string(),
            creator_sold_pct: 0.0,
            top_holder_pct: 20.0,
            liquidity_sol: 10.0,
            initial_liquidity_sol: 10.0,
            ..Default::default()
        };

        let prediction = predictor.predict(&ctx);
        assert!(prediction.warnings.is_empty());
        assert_eq!(prediction.probability, 0.0);
    }

    #[test]
    fn test_severity_weights() {
        let signal = RugWarningSignal::CreatorStartedSelling {
            pct_sold: 60.0,
            sell_count: 5,
        };
        assert!(signal.severity() > 0.8);

        let signal = RugWarningSignal::LiquidityDraining {
            drop_pct: 60.0,
            rate_per_min: 10.0,
        };
        assert!(signal.severity() > 0.9);
    }
}
