//! Dynamic Position Sizing
//!
//! Bet big on high-conviction, small on speculative.
//! Adjusts position size based on confidence, regime, liquidity, and portfolio state.

use serde::{Deserialize, Serialize};

use super::liquidity::LiquidityAnalysis;
use super::types::TokenRegime;

/// Position sizing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSizingConfig {
    pub base_size_sol: f64,
    pub min_size_sol: f64,
    pub max_size_sol: f64,
    pub confidence_scaling: bool,
}

impl Default for PositionSizingConfig {
    fn default() -> Self {
        Self {
            base_size_sol: 0.1,
            min_size_sol: 0.01,
            max_size_sol: 0.5,
            confidence_scaling: true,
        }
    }
}

/// Context for position sizing
#[derive(Debug, Clone)]
pub struct SizingContext {
    pub confidence: f64,
    pub regime: TokenRegime,
    pub liquidity: LiquidityAnalysis,
    pub portfolio_remaining_sol: f64,
    pub chain_size_factor: f64,
    pub execution_size_factor: f64,
}

impl Default for SizingContext {
    fn default() -> Self {
        // Default liquidity with reasonable values for testing
        let mut liquidity = LiquidityAnalysis::default();
        liquidity.exit_feasible = true;
        liquidity.max_safe_exit_sol = 10.0; // Generous default

        Self {
            confidence: 0.5,
            regime: TokenRegime::default(),
            liquidity,
            portfolio_remaining_sol: 1.0,
            chain_size_factor: 1.0,
            execution_size_factor: 1.0,
        }
    }
}

/// Position Sizer
pub struct PositionSizer {
    config: PositionSizingConfig,
}

impl PositionSizer {
    /// Create a new position sizer
    pub fn new(config: PositionSizingConfig) -> Self {
        Self { config }
    }

    /// Calculate position size
    pub fn calculate_size(&self, ctx: &SizingContext) -> f64 {
        let mut size = self.config.base_size_sol;

        // 1. Confidence scaling (0.5x to 2.0x)
        if self.config.confidence_scaling {
            let conf_mult = 0.5 + (ctx.confidence * 1.5);
            size *= conf_mult;
        }

        // 2. Regime scaling
        let regime_mult = ctx.regime.size_multiplier();
        size *= regime_mult;

        // If regime says avoid, return minimum
        if regime_mult == 0.0 {
            return 0.0;
        }

        // 3. Chain health factor
        size *= ctx.chain_size_factor;

        // 4. Execution quality factor
        size *= ctx.execution_size_factor;

        // 5. Liquidity constraint - don't exceed 80% of safe exit capacity
        if ctx.liquidity.exit_feasible {
            let max_safe = ctx.liquidity.max_safe_exit_sol * 0.8;
            size = size.min(max_safe);
        } else {
            // If exit isn't feasible, use minimum size
            size = self.config.min_size_sol;
        }

        // 6. Portfolio constraint
        size = size.min(ctx.portfolio_remaining_sol);

        // 7. Clamp to configured limits
        size = size.clamp(self.config.min_size_sol, self.config.max_size_sol);

        size
    }

    /// Calculate size with simple inputs
    pub fn calculate_simple(&self, confidence: f64, regime: &TokenRegime) -> f64 {
        let ctx = SizingContext {
            confidence,
            regime: regime.clone(),
            ..Default::default()
        };
        self.calculate_size(&ctx)
    }

    /// Get size breakdown for explanation
    pub fn explain_size(&self, ctx: &SizingContext) -> SizeExplanation {
        let base = self.config.base_size_sol;

        let conf_mult = if self.config.confidence_scaling {
            0.5 + (ctx.confidence * 1.5)
        } else {
            1.0
        };

        let regime_mult = ctx.regime.size_multiplier();
        let chain_mult = ctx.chain_size_factor;
        let exec_mult = ctx.execution_size_factor;

        let liquidity_cap = if ctx.liquidity.exit_feasible {
            Some(ctx.liquidity.max_safe_exit_sol * 0.8)
        } else {
            None
        };

        let portfolio_cap = ctx.portfolio_remaining_sol;

        let final_size = self.calculate_size(ctx);

        SizeExplanation {
            base_size: base,
            confidence_multiplier: conf_mult,
            regime_multiplier: regime_mult,
            chain_multiplier: chain_mult,
            execution_multiplier: exec_mult,
            liquidity_cap,
            portfolio_cap,
            final_size,
        }
    }
}

impl Default for PositionSizer {
    fn default() -> Self {
        Self::new(PositionSizingConfig::default())
    }
}

/// Explanation of how size was calculated
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeExplanation {
    pub base_size: f64,
    pub confidence_multiplier: f64,
    pub regime_multiplier: f64,
    pub chain_multiplier: f64,
    pub execution_multiplier: f64,
    pub liquidity_cap: Option<f64>,
    pub portfolio_cap: f64,
    pub final_size: f64,
}

impl std::fmt::Display for SizeExplanation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Position Size Calculation:")?;
        writeln!(f, "  Base size: {:.4} SOL", self.base_size)?;
        writeln!(f, "  × Confidence ({:.2}x)", self.confidence_multiplier)?;
        writeln!(f, "  × Regime ({:.2}x)", self.regime_multiplier)?;
        writeln!(f, "  × Chain health ({:.2}x)", self.chain_multiplier)?;
        writeln!(f, "  × Execution ({:.2}x)", self.execution_multiplier)?;
        if let Some(cap) = self.liquidity_cap {
            writeln!(f, "  Liquidity cap: {:.4} SOL", cap)?;
        }
        writeln!(f, "  Portfolio cap: {:.4} SOL", self.portfolio_cap)?;
        writeln!(f, "  = Final: {:.4} SOL", self.final_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base_size() {
        let sizer = PositionSizer::new(PositionSizingConfig {
            base_size_sol: 0.1,
            confidence_scaling: false,
            ..Default::default()
        });

        let ctx = SizingContext {
            regime: TokenRegime::OrganicPump {
                confidence: 0.5,
                expected_duration_secs: 60,
            },
            ..Default::default()
        };

        let size = sizer.calculate_size(&ctx);
        assert!((size - 0.1).abs() < 0.01);
    }

    #[test]
    fn test_confidence_scaling() {
        let sizer = PositionSizer::default();

        // Low confidence
        let low_conf = SizingContext {
            confidence: 0.3,
            regime: TokenRegime::OrganicPump {
                confidence: 0.3,
                expected_duration_secs: 60,
            },
            ..Default::default()
        };

        // High confidence
        let high_conf = SizingContext {
            confidence: 0.9,
            regime: TokenRegime::OrganicPump {
                confidence: 0.9,
                expected_duration_secs: 60,
            },
            ..Default::default()
        };

        let low_size = sizer.calculate_size(&low_conf);
        let high_size = sizer.calculate_size(&high_conf);

        assert!(high_size > low_size);
    }

    #[test]
    fn test_regime_multiplier() {
        let sizer = PositionSizer::new(PositionSizingConfig {
            confidence_scaling: false,
            ..Default::default()
        });

        // Organic pump with high confidence = 1.5x
        let organic = SizingContext {
            regime: TokenRegime::OrganicPump {
                confidence: 0.9,
                expected_duration_secs: 60,
            },
            ..Default::default()
        };

        // Sniper flip = 0.3x
        let sniper = SizingContext {
            regime: TokenRegime::SniperFlip {
                sniper_count: 5,
                expected_dump_in_secs: 30,
            },
            ..Default::default()
        };

        // Wash trade = 0x
        let wash = SizingContext {
            regime: TokenRegime::WashTrade {
                wash_pct: 0.8,
                real_volume_sol: 0.5,
            },
            ..Default::default()
        };

        let organic_size = sizer.calculate_size(&organic);
        let sniper_size = sizer.calculate_size(&sniper);
        let wash_size = sizer.calculate_size(&wash);

        assert!(organic_size > sniper_size);
        assert_eq!(wash_size, 0.0);
    }

    #[test]
    fn test_liquidity_cap() {
        let sizer = PositionSizer::new(PositionSizingConfig {
            base_size_sol: 1.0, // Large base
            max_size_sol: 2.0,
            min_size_sol: 0.01,
            confidence_scaling: false,
        });

        let mut liquidity = LiquidityAnalysis::default();
        liquidity.exit_feasible = true;
        liquidity.max_safe_exit_sol = 0.2; // Can only safely exit 0.2 SOL

        // Explicit context to avoid default interference
        let ctx = SizingContext {
            confidence: 0.5,
            regime: TokenRegime::OrganicPump {
                confidence: 0.8,
                expected_duration_secs: 60,
            },
            liquidity,
            portfolio_remaining_sol: 10.0,
            chain_size_factor: 1.0,
            execution_size_factor: 1.0,
        };

        let size = sizer.calculate_size(&ctx);
        // Should be capped at 0.2 * 0.8 = 0.16 SOL (with small epsilon for float comparison)
        assert!(size <= 0.161, "Expected size <= 0.16, got {}", size);
    }

    #[test]
    fn test_portfolio_cap() {
        let sizer = PositionSizer::new(PositionSizingConfig {
            base_size_sol: 1.0,
            max_size_sol: 2.0,
            confidence_scaling: false,
            ..Default::default()
        });

        let ctx = SizingContext {
            portfolio_remaining_sol: 0.3,
            regime: TokenRegime::OrganicPump {
                confidence: 0.8,
                expected_duration_secs: 60,
            },
            ..Default::default()
        };

        let size = sizer.calculate_size(&ctx);
        assert!(size <= 0.3);
    }

    #[test]
    fn test_explain_size() {
        let sizer = PositionSizer::default();

        let ctx = SizingContext {
            confidence: 0.8,
            regime: TokenRegime::OrganicPump {
                confidence: 0.8,
                expected_duration_secs: 60,
            },
            ..Default::default()
        };

        let explanation = sizer.explain_size(&ctx);
        assert!(explanation.final_size > 0.0);
        assert!(explanation.confidence_multiplier > 1.0);
    }
}
