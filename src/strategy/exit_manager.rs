//! Adaptive Exit Manager
//!
//! Exit style depends on position state and market conditions.
//! Supports quick scalps, tiered exits, trailing stops, and condition-based exits.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::delta_tracker::DeltaMetrics;
use super::price_action::PriceAction;
use super::regime::RegimeClassification;
use super::types::{
    ExitCondition, ExitReason, ExitSignal, ExitStyle, Position, TradingStrategy, Urgency,
};

/// Exit manager configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitManagerConfig {
    pub default_style: String,
    pub quick_scalp_target_pct: f64,
    pub tiered_levels: Vec<(f64, f64)>, // (gain_pct, sell_pct)
    pub trailing_stop_pct: f64,
    pub trailing_activation_pct: f64,
    pub max_hold_secs: u64,
    pub stop_loss_pct: f64,
}

impl Default for ExitManagerConfig {
    fn default() -> Self {
        Self {
            default_style: "adaptive".to_string(),
            quick_scalp_target_pct: 25.0,
            tiered_levels: vec![(50.0, 50.0), (100.0, 25.0), (200.0, 25.0)],
            trailing_stop_pct: 15.0,
            trailing_activation_pct: 30.0,
            max_hold_secs: 300,
            stop_loss_pct: 15.0,
        }
    }
}

/// Position context for exit decisions
#[derive(Debug, Clone)]
pub struct PositionContext {
    pub position: Position,
    pub current_price: f64,
    pub high_price: f64,
    pub pnl_pct: f64,
    pub hold_time_secs: u64,
    pub entry_strategy: TradingStrategy,
    pub regime: RegimeClassification,
    pub delta: DeltaMetrics,
    pub price_action: PriceAction,
    pub levels_hit: Vec<f64>,
}

impl PositionContext {
    /// Check if a tiered exit level has been hit
    pub fn level_hit(&self, target: f64) -> bool {
        self.levels_hit.contains(&target)
    }
}

/// Exit manager for adaptive exit selection
pub struct ExitManager {
    config: ExitManagerConfig,
    position_high_prices: HashMap<String, f64>,
    position_levels_hit: HashMap<String, Vec<f64>>,
}

impl ExitManager {
    /// Create a new exit manager
    pub fn new(config: ExitManagerConfig) -> Self {
        Self {
            config,
            position_high_prices: HashMap::new(),
            position_levels_hit: HashMap::new(),
        }
    }

    /// Update position tracking with new price
    pub fn update_price(&mut self, mint: &str, price: f64) {
        let high = self
            .position_high_prices
            .entry(mint.to_string())
            .or_insert(price);
        if price > *high {
            *high = price;
        }
    }

    /// Get high price for position
    pub fn get_high_price(&self, mint: &str) -> f64 {
        self.position_high_prices.get(mint).copied().unwrap_or(0.0)
    }

    /// Mark a tiered level as hit
    pub fn mark_level_hit(&mut self, mint: &str, level: f64) {
        self.position_levels_hit
            .entry(mint.to_string())
            .or_default()
            .push(level);
    }

    /// Get levels hit for a position
    pub fn get_levels_hit(&self, mint: &str) -> Vec<f64> {
        self.position_levels_hit
            .get(mint)
            .cloned()
            .unwrap_or_default()
    }

    /// Clear tracking for a position
    pub fn clear_position(&mut self, mint: &str) {
        self.position_high_prices.remove(mint);
        self.position_levels_hit.remove(mint);
    }

    /// Select exit style based on position context
    pub fn select_exit_style(&self, ctx: &PositionContext) -> ExitStyle {
        match ctx.entry_strategy {
            TradingStrategy::SnipeAndScalp => ExitStyle::QuickScalp {
                target_pct: self.config.quick_scalp_target_pct,
            },

            TradingStrategy::MomentumSurfing => {
                if ctx.regime.confidence > 0.8 && ctx.regime.should_enter {
                    // High confidence organic = let it run with tiered exits
                    ExitStyle::TieredExit {
                        levels: self.config.tiered_levels.clone(),
                    }
                } else {
                    // Lower confidence = quick exit
                    ExitStyle::QuickScalp { target_pct: 30.0 }
                }
            }

            TradingStrategy::WhaleFollowing => {
                // Mirror whale exit behavior
                ExitStyle::ConditionBased {
                    exit_on: vec![ExitCondition::WhaleExits],
                }
            }

            TradingStrategy::Adaptive => {
                // Adaptive style based on conditions
                self.select_adaptive_style(ctx)
            }
        }
    }

    /// Select adaptive exit style based on multiple factors
    fn select_adaptive_style(&self, ctx: &PositionContext) -> ExitStyle {
        // If we're in profit and price structure is breaking down
        if ctx.pnl_pct > 20.0 && ctx.price_action.lower_highs {
            return ExitStyle::QuickScalp {
                target_pct: ctx.pnl_pct, // Lock in current gains
            };
        }

        // If momentum is fading, use trailing stop
        if ctx.pnl_pct > self.config.trailing_activation_pct {
            return ExitStyle::TrailingStop {
                trail_pct: self.config.trailing_stop_pct,
                activation_pct: self.config.trailing_activation_pct,
            };
        }

        // If high confidence regime, use tiered exits
        if ctx.regime.confidence > 0.7 {
            return ExitStyle::TieredExit {
                levels: self.config.tiered_levels.clone(),
            };
        }

        // Default to quick scalp
        ExitStyle::QuickScalp {
            target_pct: self.config.quick_scalp_target_pct,
        }
    }

    /// Check if position should exit
    pub fn should_exit(&self, ctx: &PositionContext) -> Option<ExitSignal> {
        // First check stop loss
        if ctx.pnl_pct <= -self.config.stop_loss_pct {
            return Some(ExitSignal {
                mint: ctx.position.mint.clone(),
                pct_to_sell: 100.0,
                reason: ExitReason::StopLoss {
                    loss_pct: -ctx.pnl_pct,
                },
                urgency: Urgency::Immediate,
            });
        }

        // Check max hold time
        if ctx.hold_time_secs >= self.config.max_hold_secs {
            return Some(ExitSignal {
                mint: ctx.position.mint.clone(),
                pct_to_sell: 100.0,
                reason: ExitReason::MaxHoldTime {
                    held_secs: ctx.hold_time_secs,
                },
                urgency: Urgency::High,
            });
        }

        // Check condition-based exits
        if let Some(signal) = self.check_conditions(ctx) {
            return Some(signal);
        }

        // Check style-specific exits
        let style = self.select_exit_style(ctx);
        self.check_style_exit(ctx, &style)
    }

    /// Check condition-based exit triggers
    fn check_conditions(&self, ctx: &PositionContext) -> Option<ExitSignal> {
        // Momentum fade detection (use price_velocity as proxy for momentum)
        if ctx.delta.price_velocity < -0.5 && ctx.pnl_pct > 10.0 {
            return Some(ExitSignal {
                mint: ctx.position.mint.clone(),
                pct_to_sell: 100.0,
                reason: ExitReason::MomentumFade,
                urgency: Urgency::High,
            });
        }

        // Distribution worsening - exit partially to lock in gains
        if ctx.delta.top_holder_pct_delta > 5.0 && ctx.pnl_pct > 0.0 {
            return Some(ExitSignal {
                mint: ctx.position.mint.clone(),
                pct_to_sell: 50.0,
                reason: ExitReason::MomentumFade, // Use MomentumFade as closest match
                urgency: Urgency::Normal,
            });
        }

        // Price structure breaking
        if ctx.price_action.lower_highs && ctx.pnl_pct > 15.0 {
            return Some(ExitSignal {
                mint: ctx.position.mint.clone(),
                pct_to_sell: 75.0,
                reason: ExitReason::MomentumFade, // Price structure break signals momentum fade
                urgency: Urgency::High,
            });
        }

        None
    }

    /// Check style-specific exit conditions
    fn check_style_exit(&self, ctx: &PositionContext, style: &ExitStyle) -> Option<ExitSignal> {
        match style {
            ExitStyle::QuickScalp { target_pct } => {
                if ctx.pnl_pct >= *target_pct {
                    return Some(ExitSignal {
                        mint: ctx.position.mint.clone(),
                        pct_to_sell: 100.0,
                        reason: ExitReason::TakeProfit {
                            pnl_pct: ctx.pnl_pct,
                        },
                        urgency: Urgency::High,
                    });
                }
            }

            ExitStyle::TieredExit { levels } => {
                for (target, sell_pct) in levels {
                    if ctx.pnl_pct >= *target && !ctx.level_hit(*target) {
                        return Some(ExitSignal {
                            mint: ctx.position.mint.clone(),
                            pct_to_sell: *sell_pct,
                            reason: ExitReason::TakeProfit {
                                pnl_pct: ctx.pnl_pct,
                            },
                            urgency: Urgency::Normal,
                        });
                    }
                }
            }

            ExitStyle::TrailingStop {
                trail_pct,
                activation_pct,
            } => {
                if ctx.pnl_pct >= *activation_pct {
                    let trail_price = ctx.high_price * (1.0 - trail_pct / 100.0);
                    if ctx.current_price < trail_price {
                        return Some(ExitSignal {
                            mint: ctx.position.mint.clone(),
                            pct_to_sell: 100.0,
                            reason: ExitReason::TrailingStopHit {
                                peak_pnl_pct: ((ctx.high_price - ctx.position.entry_price)
                                    / ctx.position.entry_price)
                                    * 100.0,
                                current_pnl_pct: ctx.pnl_pct,
                            },
                            urgency: Urgency::Immediate,
                        });
                    }
                }
            }

            ExitStyle::TimeBased { max_hold_secs } => {
                if ctx.hold_time_secs >= *max_hold_secs {
                    return Some(ExitSignal {
                        mint: ctx.position.mint.clone(),
                        pct_to_sell: 100.0,
                        reason: ExitReason::MaxHoldTime {
                            held_secs: ctx.hold_time_secs,
                        },
                        urgency: Urgency::High,
                    });
                }
            }

            ExitStyle::ConditionBased { exit_on } => {
                for condition in exit_on {
                    if self.check_condition(ctx, condition) {
                        return Some(ExitSignal {
                            mint: ctx.position.mint.clone(),
                            pct_to_sell: 100.0,
                            reason: ExitReason::MomentumFade, // Generic exit condition
                            urgency: Urgency::High,
                        });
                    }
                }
            }
        }

        None
    }

    /// Check a specific exit condition
    fn check_condition(&self, ctx: &PositionContext, condition: &ExitCondition) -> bool {
        match condition {
            ExitCondition::MomentumFade => ctx.delta.price_velocity < -0.3,
            ExitCondition::WhaleExits => false, // Would need whale tracking
            ExitCondition::DistributionWorsens => ctx.delta.top_holder_pct_delta > 5.0,
            ExitCondition::CreatorSelling => false, // Would need creator tracking
            ExitCondition::PriceStructureBreaks => ctx.price_action.lower_highs,
            ExitCondition::StopLossHit => ctx.pnl_pct <= -self.config.stop_loss_pct,
            ExitCondition::MaxHoldTimeReached => ctx.hold_time_secs >= self.config.max_hold_secs,
        }
    }

    /// Calculate recommended exit percentage for partial exits
    pub fn calculate_exit_percentage(&self, ctx: &PositionContext) -> f64 {
        let style = self.select_exit_style(ctx);

        match style {
            ExitStyle::QuickScalp { .. } => 100.0,
            ExitStyle::TieredExit { levels } => {
                // Find next level to hit
                for (target, sell_pct) in levels {
                    if ctx.pnl_pct >= target && !ctx.level_hit(target) {
                        return sell_pct;
                    }
                }
                0.0
            }
            ExitStyle::TrailingStop { .. } => 100.0,
            ExitStyle::TimeBased { .. } => 100.0,
            ExitStyle::ConditionBased { .. } => 100.0,
        }
    }

    /// Get exit style explanation
    pub fn explain_exit_style(&self, ctx: &PositionContext) -> String {
        let style = self.select_exit_style(ctx);

        match style {
            ExitStyle::QuickScalp { target_pct } => {
                format!("Quick Scalp: Exit at {:.1}% profit", target_pct)
            }
            ExitStyle::TieredExit { levels } => {
                let level_str: Vec<String> = levels
                    .iter()
                    .map(|(t, p)| format!("{:.0}%@{:.0}%", p, t))
                    .collect();
                format!("Tiered Exit: {}", level_str.join(", "))
            }
            ExitStyle::TrailingStop {
                trail_pct,
                activation_pct,
            } => {
                format!(
                    "Trailing Stop: {:.1}% trail after {:.1}% gain",
                    trail_pct, activation_pct
                )
            }
            ExitStyle::TimeBased { max_hold_secs } => {
                format!("Time Based: Exit after {}s", max_hold_secs)
            }
            ExitStyle::ConditionBased { exit_on } => {
                let conditions: Vec<String> = exit_on.iter().map(|c| format!("{:?}", c)).collect();
                format!("Condition Based: {}", conditions.join(", "))
            }
        }
    }
}

impl Default for ExitManager {
    fn default() -> Self {
        Self::new(ExitManagerConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::TokenRegime;
    use super::*;

    fn create_test_context(pnl_pct: f64, strategy: TradingStrategy) -> PositionContext {
        let entry_price = 0.001;
        let current_price = entry_price * (1.0 + pnl_pct / 100.0);
        PositionContext {
            position: Position {
                mint: "test_mint".to_string(),
                size_sol: 0.1,
                entry_price,
                entry_time: chrono::Utc::now(),
                tokens_held: 100_000,
                strategy: strategy.clone(),
                exit_style: ExitStyle::default(),
                highest_price: current_price,
                lowest_price: entry_price,
                exit_levels_hit: vec![],
            },
            current_price,
            high_price: current_price,
            pnl_pct,
            hold_time_secs: 30,
            entry_strategy: strategy,
            regime: RegimeClassification {
                regime: TokenRegime::OrganicPump {
                    confidence: 0.85,
                    expected_duration_secs: 60,
                },
                confidence: 0.85, // Must be > 0.8 for TieredExit to trigger
                reasons: vec![],
                should_enter: true,
                size_multiplier: 1.0,
            },
            delta: DeltaMetrics::default(),
            price_action: PriceAction::default(),
            levels_hit: vec![],
        }
    }

    #[test]
    fn test_quick_scalp_exit() {
        let exit_manager = ExitManager::default();
        let ctx = create_test_context(30.0, TradingStrategy::SnipeAndScalp);

        let signal = exit_manager.should_exit(&ctx);
        assert!(signal.is_some());
        let sig = signal.unwrap();
        assert!(matches!(sig.reason, ExitReason::TakeProfit { .. }));
    }

    #[test]
    fn test_stop_loss() {
        let exit_manager = ExitManager::default();
        let ctx = create_test_context(-20.0, TradingStrategy::MomentumSurfing);

        let signal = exit_manager.should_exit(&ctx);
        assert!(signal.is_some());
        let sig = signal.unwrap();
        assert!(matches!(sig.reason, ExitReason::StopLoss { .. }));
    }

    #[test]
    fn test_tiered_exit() {
        let exit_manager = ExitManager::default();
        let ctx = create_test_context(55.0, TradingStrategy::MomentumSurfing);

        let signal = exit_manager.should_exit(&ctx);
        assert!(signal.is_some());
        let sig = signal.unwrap();
        // Tiered exit returns TakeProfit reason
        assert!(matches!(sig.reason, ExitReason::TakeProfit { .. }));
        assert!(sig.pct_to_sell < 100.0); // Partial exit
    }

    #[test]
    fn test_trailing_stop() {
        let exit_manager = ExitManager::default();
        let mut ctx = create_test_context(40.0, TradingStrategy::Adaptive);
        ctx.high_price = 0.002; // Price was higher
        ctx.current_price = 0.0016; // Dropped 20% from high

        let style = exit_manager.select_exit_style(&ctx);
        assert!(matches!(style, ExitStyle::TrailingStop { .. }));
    }

    #[test]
    fn test_max_hold_time() {
        let exit_manager = ExitManager::default();
        let mut ctx = create_test_context(5.0, TradingStrategy::MomentumSurfing);
        ctx.hold_time_secs = 400; // Exceeds default 300s

        let signal = exit_manager.should_exit(&ctx);
        assert!(signal.is_some());
        let sig = signal.unwrap();
        assert!(matches!(sig.reason, ExitReason::MaxHoldTime { .. }));
    }

    #[test]
    fn test_select_style_snipe() {
        let exit_manager = ExitManager::default();
        let ctx = create_test_context(10.0, TradingStrategy::SnipeAndScalp);

        let style = exit_manager.select_exit_style(&ctx);
        assert!(matches!(style, ExitStyle::QuickScalp { .. }));
    }

    #[test]
    fn test_select_style_whale() {
        let exit_manager = ExitManager::default();
        let ctx = create_test_context(10.0, TradingStrategy::WhaleFollowing);

        let style = exit_manager.select_exit_style(&ctx);
        assert!(matches!(style, ExitStyle::ConditionBased { .. }));
    }

    #[test]
    fn test_high_price_tracking() {
        let mut exit_manager = ExitManager::default();

        exit_manager.update_price("mint1", 0.001);
        exit_manager.update_price("mint1", 0.002);
        exit_manager.update_price("mint1", 0.0015);

        assert!((exit_manager.get_high_price("mint1") - 0.002).abs() < 0.0001);
    }

    #[test]
    fn test_level_tracking() {
        let mut exit_manager = ExitManager::default();

        exit_manager.mark_level_hit("mint1", 50.0);
        exit_manager.mark_level_hit("mint1", 100.0);

        let levels = exit_manager.get_levels_hit("mint1");
        assert_eq!(levels.len(), 2);
        assert!(levels.contains(&50.0));
        assert!(levels.contains(&100.0));
    }

    #[test]
    fn test_clear_position() {
        let mut exit_manager = ExitManager::default();

        exit_manager.update_price("mint1", 0.002);
        exit_manager.mark_level_hit("mint1", 50.0);

        exit_manager.clear_position("mint1");

        assert_eq!(exit_manager.get_high_price("mint1"), 0.0);
        assert!(exit_manager.get_levels_hit("mint1").is_empty());
    }
}
