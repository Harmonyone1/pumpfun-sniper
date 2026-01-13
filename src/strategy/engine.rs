//! Strategy Engine
//!
//! Main coordinator for the aggressive trading strategy system.
//! Brings together all modules: risk, regime, sizing, exits, and tactics.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::arbitrator::DecisionArbitrator;
use super::chain_health::{ChainHealth, ChainHealthConfig};
use super::delta_tracker::DeltaTracker;
use super::execution_feedback::{ExecutionFeedback, ExecutionFeedbackConfig};
use super::exit_manager::{ExitManager, ExitManagerConfig, PositionContext};
use super::fatal_risk::{FatalRiskContext, FatalRiskEngine, FatalRiskConfig};
use super::liquidity::{LiquidityAnalyzer, LiquidityConfig};
use super::portfolio_risk::{PortfolioRiskGovernor, PortfolioRiskConfig};
use super::price_action::{PriceAction, PriceActionAnalyzer};
use super::randomization::{RandomizationConfig, Randomizer};
use super::regime::{
    CreatorBehavior, OrderFlowAnalysis, RegimeClassification, RegimeClassifier, TokenDistribution,
};
use super::sizing::{PositionSizer, PositionSizingConfig, SizingContext};
use super::types::{
    ArbitratedDecision, DecisionExplanation, EntrySignal, ExitSignal, Position,
    TokenRegime, TradingAction, TradingStrategy,
};

/// Strategy engine configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyEngineConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub default_strategy: TradingStrategy,
    #[serde(default)]
    pub position_sizing: PositionSizingConfig,
    #[serde(default)]
    pub exits: ExitManagerConfig,
    #[serde(default)]
    pub fatal_risks: FatalRiskConfig,
    #[serde(default)]
    pub portfolio_risk: PortfolioRiskConfig,
    #[serde(default)]
    pub chain_health: ChainHealthConfig,
    #[serde(default)]
    pub execution_feedback: ExecutionFeedbackConfig,
    #[serde(default)]
    pub randomization: RandomizationConfig,
    #[serde(default)]
    pub liquidity: LiquidityConfig,
}

fn default_enabled() -> bool {
    true
}

impl Default for StrategyEngineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_strategy: TradingStrategy::Adaptive,
            position_sizing: PositionSizingConfig::default(),
            exits: ExitManagerConfig::default(),
            fatal_risks: FatalRiskConfig::default(),
            portfolio_risk: PortfolioRiskConfig::default(),
            chain_health: ChainHealthConfig::default(),
            execution_feedback: ExecutionFeedbackConfig::default(),
            randomization: RandomizationConfig::default(),
            liquidity: LiquidityConfig::default(),
        }
    }
}

/// Token analysis context
#[derive(Debug, Clone)]
pub struct TokenAnalysisContext {
    pub mint: String,
    pub order_flow: OrderFlowAnalysis,
    pub distribution: TokenDistribution,
    pub creator_behavior: CreatorBehavior,
    pub price_action: PriceAction,
    pub sol_reserves: f64,
    pub token_reserves: f64,
    pub confidence_score: f64,
}

/// Entry evaluation result
#[derive(Debug, Clone)]
pub struct EntryEvaluation {
    pub decision: ArbitratedDecision,
    pub regime: RegimeClassification,
    pub position_size: f64,
    pub explanation: DecisionExplanation,
}

/// Position evaluation result
#[derive(Debug, Clone)]
pub struct PositionEvaluation {
    pub exit_signal: Option<ExitSignal>,
    pub current_pnl_pct: f64,
    pub regime: RegimeClassification,
    pub recommendation: String,
}

/// Main Strategy Engine
pub struct StrategyEngine {
    config: StrategyEngineConfig,

    // Core components
    fatal_risk: FatalRiskEngine,
    liquidity: LiquidityAnalyzer,
    portfolio_risk: Arc<RwLock<PortfolioRiskGovernor>>,
    chain_health: Arc<RwLock<ChainHealth>>,
    execution_feedback: Arc<RwLock<ExecutionFeedback>>,

    // Analysis components
    regime_classifier: RegimeClassifier,
    position_sizer: PositionSizer,
    exit_manager: Arc<RwLock<ExitManager>>,
    arbitrator: DecisionArbitrator,
    randomizer: Arc<RwLock<Randomizer>>,

    // Per-token trackers
    delta_trackers: HashMap<String, DeltaTracker>,
    price_analyzers: HashMap<String, PriceActionAnalyzer>,
}

impl StrategyEngine {
    /// Create a new strategy engine
    pub fn new(config: StrategyEngineConfig) -> Self {
        Self {
            fatal_risk: FatalRiskEngine::new(config.fatal_risks.clone()),
            liquidity: LiquidityAnalyzer::new(config.liquidity.clone()),
            portfolio_risk: Arc::new(RwLock::new(PortfolioRiskGovernor::new(
                config.portfolio_risk.clone(),
            ))),
            chain_health: Arc::new(RwLock::new(ChainHealth::new(config.chain_health.clone()))),
            execution_feedback: Arc::new(RwLock::new(ExecutionFeedback::new(
                config.execution_feedback.clone(),
            ))),
            regime_classifier: RegimeClassifier::new(),
            position_sizer: PositionSizer::new(config.position_sizing.clone()),
            exit_manager: Arc::new(RwLock::new(ExitManager::new(config.exits.clone()))),
            arbitrator: DecisionArbitrator::default(),
            randomizer: Arc::new(RwLock::new(Randomizer::from_entropy(
                config.randomization.clone(),
            ))),
            delta_trackers: HashMap::new(),
            price_analyzers: HashMap::new(),
            config,
        }
    }

    /// Check if strategy engine is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Evaluate a token for potential entry
    pub async fn evaluate_entry(&mut self, ctx: &TokenAnalysisContext) -> EntryEvaluation {
        // 1. Build fatal risk context
        let creator_sell_info = if ctx.creator_behavior.total_sold_pct > 0.0 {
            Some((ctx.creator_behavior.total_sold_pct, ctx.creator_behavior.avg_sell_interval_secs))
        } else {
            None
        };

        let fatal_context = FatalRiskContext {
            mint: ctx.mint.clone(),
            creator: String::new(), // Would come from token data
            mint_authority_active: false, // Would come from RPC
            freeze_authority_active: false,
            creator_sell_info,
            effective_liquidity_sol: ctx.sol_reserves,
            exit_slippage_pct: 0.0, // Would calculate from liquidity
            min_position_sol: 0.01,
            liquidity_drop_pct: None, // Would track over time
            wash_trading_score: ctx.order_flow.wash_trading_score,
            failed_sell_count: 0,
            price_drop_from_ath: 0.0,
            chain_congestion_critical: false,
        };

        // 2. Check fatal risks
        let fatal_result = self.fatal_risk.check(&fatal_context).await;

        // 3. Get chain state
        let chain_health = self.chain_health.read().await;
        let chain_state = chain_health.get_state();
        drop(chain_health);

        // 4. Check portfolio risk
        let delta_metrics = self.get_or_create_delta_tracker(&ctx.mint).compute_metrics(&ctx.mint);
        let regime = self.regime_classifier.classify(
            &ctx.order_flow,
            &ctx.distribution,
            &ctx.creator_behavior,
            &delta_metrics,
        );

        // Calculate preliminary size for portfolio check
        let liquidity = self.liquidity.analyze_simple(ctx.sol_reserves, ctx.token_reserves);

        let exec_feedback = self.execution_feedback.read().await;
        let _exec_quality = exec_feedback.get_quality();
        drop(exec_feedback);

        // Get chain size factor
        let chain_health_ref = self.chain_health.read().await;
        let chain_size_factor = chain_health_ref.get_size_multiplier();
        drop(chain_health_ref);

        // Get execution size factor
        let exec_feedback_ref = self.execution_feedback.read().await;
        let execution_size_factor = exec_feedback_ref.get_size_factor();
        drop(exec_feedback_ref);

        let sizing_ctx = SizingContext {
            confidence: ctx.confidence_score,
            regime: regime.regime.clone(),
            liquidity: liquidity.clone(),
            portfolio_remaining_sol: 1.0, // Will be updated
            chain_size_factor,
            execution_size_factor,
        };

        let position_size = self.position_sizer.calculate_size(&sizing_ctx);

        // Check portfolio limits
        let portfolio = self.portfolio_risk.read().await;
        let portfolio_result = portfolio.can_open_position(position_size);
        let _portfolio_state = portfolio.get_state();
        drop(portfolio);

        // 5. Create entry signal if conditions are met
        let strategy_signal = if regime.should_enter && ctx.confidence_score > 0.5 {
            Some(EntrySignal {
                mint: ctx.mint.clone(),
                strategy: self.config.default_strategy.clone(),
                confidence: ctx.confidence_score,
                suggested_size_sol: position_size,
                urgency: super::types::Urgency::Normal,
                max_price: None,
                reason: regime.reasons.join(", "),
            })
        } else {
            None
        };

        // 6. Arbitrate decision
        let decision = self.arbitrator.arbitrate_entry(
            &ctx.mint,
            fatal_result,
            &chain_state.recommended_action,
            portfolio_result,
            strategy_signal,
            &regime.regime,
        );

        // 7. Apply randomization if entering
        let final_size = if matches!(decision.action, TradingAction::Enter { .. }) {
            let mut randomizer = self.randomizer.write().await;
            let jittered = randomizer.jitter_entry(position_size);

            if jittered.should_skip {
                // Randomly skip for adversarial resistance
                return EntryEvaluation {
                    decision: ArbitratedDecision {
                        action: TradingAction::Skip {
                            reason: "Random skip for adversarial resistance".to_string(),
                        },
                        ..decision
                    },
                    regime: regime.clone(),
                    position_size: 0.0,
                    explanation: self.build_explanation(&ctx, &regime, 0.0, &chain_state),
                };
            }

            jittered.size
        } else {
            position_size
        };

        // 8. Build explanation
        let explanation = self.build_explanation(&ctx, &regime, final_size, &chain_state);

        EntryEvaluation {
            decision,
            regime,
            position_size: final_size,
            explanation,
        }
    }

    /// Evaluate an existing position for potential exit
    pub async fn evaluate_position(&mut self, position: &Position) -> PositionEvaluation {
        // Get current price and calculate PnL
        let current_price = self.get_current_price(&position.mint).unwrap_or(position.entry_price);
        let pnl_pct = if position.entry_price > 0.0 {
            ((current_price - position.entry_price) / position.entry_price) * 100.0
        } else {
            0.0
        };

        // Get delta metrics
        let delta = self
            .get_or_create_delta_tracker(&position.mint)
            .compute_metrics(&position.mint);

        // Get price action
        let price_action = self
            .get_or_create_price_analyzer(&position.mint)
            .analyze();

        // Get regime (simplified - would need full context in real impl)
        let regime = RegimeClassification {
            regime: TokenRegime::Unknown {
                data_completeness: 0.5,
            },
            confidence: 0.5,
            reasons: vec![],
            should_enter: false,
            size_multiplier: 1.0,
        };

        // Build position context
        let exit_manager = self.exit_manager.read().await;
        let high_price = exit_manager.get_high_price(&position.mint);
        let levels_hit = exit_manager.get_levels_hit(&position.mint);
        drop(exit_manager);

        let hold_time = chrono::Utc::now()
            .signed_duration_since(position.entry_time)
            .num_seconds() as u64;

        let ctx = PositionContext {
            position: position.clone(),
            current_price,
            high_price: if high_price > 0.0 {
                high_price
            } else {
                current_price
            },
            pnl_pct,
            hold_time_secs: hold_time,
            entry_strategy: position.strategy.clone(),
            regime: regime.clone(),
            delta,
            price_action,
            levels_hit,
        };

        // Check exit conditions
        let exit_manager = self.exit_manager.read().await;
        let exit_signal = exit_manager.should_exit(&ctx);
        let recommendation = exit_manager.explain_exit_style(&ctx);
        drop(exit_manager);

        // Update high price tracking
        let mut exit_manager = self.exit_manager.write().await;
        exit_manager.update_price(&position.mint, current_price);
        drop(exit_manager);

        PositionEvaluation {
            exit_signal,
            current_pnl_pct: pnl_pct,
            regime,
            recommendation,
        }
    }

    /// Record a successful entry
    pub async fn record_entry(&mut self, position: Position) {
        let mut portfolio = self.portfolio_risk.write().await;
        portfolio.open_position(position);
    }

    /// Record a successful exit
    pub async fn record_exit(&mut self, mint: &str, pnl_sol: f64) {
        // Update portfolio
        let mut portfolio = self.portfolio_risk.write().await;
        portfolio.close_position(mint, pnl_sol);
        drop(portfolio);

        // Clear exit manager tracking
        let mut exit_manager = self.exit_manager.write().await;
        exit_manager.clear_position(mint);
        drop(exit_manager);

        // Clean up trackers
        self.delta_trackers.remove(mint);
        self.price_analyzers.remove(mint);
    }

    /// Record execution result for feedback
    pub async fn record_execution(
        &mut self,
        mint: &str,
        is_buy: bool,
        size_sol: f64,
        expected_price: f64,
        actual_price: f64,
        latency_ms: u64,
        tx_sig: &str,
    ) {
        let mut feedback = self.execution_feedback.write().await;
        if is_buy {
            feedback.record_buy(mint, size_sol, expected_price, actual_price, latency_ms, tx_sig);
        } else {
            feedback.record_sell(mint, size_sol, expected_price, actual_price, latency_ms, tx_sig);
        }

        // Also record in chain health
        let mut chain_health = self.chain_health.write().await;
        chain_health.record_tx(true);
    }

    /// Record a failed transaction
    pub async fn record_tx_failure(&mut self, mint: &str, is_buy: bool, size_sol: f64, latency_ms: u64, reason: &str) {
        let mut feedback = self.execution_feedback.write().await;
        feedback.record_failure(
            mint,
            if is_buy {
                super::types::Side::Buy
            } else {
                super::types::Side::Sell
            },
            size_sol,
            latency_ms,
            reason,
        );

        let mut chain_health = self.chain_health.write().await;
        chain_health.record_tx(false);
    }

    /// Update price data for a token
    pub fn update_price(&mut self, mint: &str, price: f64, volume: f64) {
        // Update price action analyzer
        let analyzer = self.get_or_create_price_analyzer(mint);
        analyzer.record_price(price, volume);
    }

    /// Update delta tracker for a token
    pub fn update_metrics(
        &mut self,
        mint: &str,
        holder_count: i32,
        top_holder_pct: f64,
        volume_sol: f64,
        net_flow_sol: f64,
    ) {
        let tracker = self.get_or_create_delta_tracker(mint);
        let prefix = format!("{}:", mint);
        tracker.record(&format!("{}holders", prefix), holder_count as f64);
        tracker.record(&format!("{}holders_5m", prefix), holder_count as f64);
        tracker.record(&format!("{}top_holder_pct", prefix), top_holder_pct);
        tracker.record(&format!("{}volume", prefix), volume_sol);
        tracker.record(&format!("{}net_flow", prefix), net_flow_sol);
    }

    /// Set filter cache for sharing with fatal risk engine
    pub fn set_filter_cache(&mut self, cache: std::sync::Arc<crate::filter::cache::FilterCache>) {
        self.fatal_risk = super::fatal_risk::FatalRiskEngine::with_cache(
            self.config.fatal_risks.clone(),
            cache,
        );
    }

    /// Check for exit signals on an existing position
    pub async fn check_exit(
        &mut self,
        mint: &str,
        entry_price: f64,
        current_price: f64,
        pnl_pct: f64,
        hold_time_secs: u64,
    ) -> Option<super::types::ExitSignal> {
        use super::exit_manager::PositionContext;
        use super::types::{ExitStyle, Position};

        // Get or create analyzers for this token
        let delta_metrics = self.get_or_create_delta_tracker(mint).compute_metrics(mint);
        let price_action = self.get_or_create_price_analyzer(mint).analyze();

        // Create a minimal position for the exit manager
        let position = Position {
            mint: mint.to_string(),
            entry_price,
            entry_time: chrono::Utc::now() - chrono::Duration::seconds(hold_time_secs as i64),
            size_sol: 0.1, // Placeholder
            tokens_held: 0,
            strategy: self.config.default_strategy.clone(),
            exit_style: ExitStyle::default(),
            highest_price: current_price,
            lowest_price: entry_price.min(current_price),
            exit_levels_hit: vec![],
        };

        // Get regime classification with default values
        let regime = super::regime::RegimeClassification {
            regime: super::types::TokenRegime::OrganicPump {
                confidence: 0.5,
                expected_duration_secs: 60,
            },
            confidence: 0.5,
            reasons: vec![],
            should_enter: false,
            size_multiplier: 1.0,
        };

        // Get exit manager
        let mut exit_manager = self.exit_manager.write().await;

        // Build position context
        let high_price = exit_manager.get_high_price(mint).max(current_price);
        let levels_hit = exit_manager.get_levels_hit(mint);

        let ctx = PositionContext {
            position,
            current_price,
            high_price,
            pnl_pct,
            hold_time_secs,
            entry_strategy: self.config.default_strategy.clone(),
            regime,
            delta: delta_metrics,
            price_action,
            levels_hit,
        };

        // Update high price
        exit_manager.update_price(mint, current_price);

        // Check for exit signal
        exit_manager.should_exit(&ctx)
    }

    /// Get entry delay for randomization
    pub async fn get_entry_delay(&self) -> std::time::Duration {
        let mut randomizer = self.randomizer.write().await;
        randomizer.jitter_entry_delay()
    }

    /// Get exit delay for randomization
    pub async fn get_exit_delay(&self) -> std::time::Duration {
        let mut randomizer = self.randomizer.write().await;
        randomizer.jitter_exit_delay()
    }

    /// Check if trading should be paused, returning the reason if paused
    pub async fn should_pause_trading_with_reason(&self) -> Option<String> {
        // Check chain health
        let chain_health = self.chain_health.read().await;
        let chain_state = chain_health.get_state();
        tracing::debug!(
            "Chain health check: congestion={:?}, block_entries={}",
            chain_state.congestion_level,
            chain_health.should_block_entries()
        );
        if chain_health.should_block_entries() {
            return Some(format!("Chain congestion: {:?}", chain_state.congestion_level));
        }
        drop(chain_health);

        // Check execution quality
        let exec_feedback = self.execution_feedback.read().await;
        let quality = exec_feedback.get_quality();
        tracing::debug!(
            "Execution quality check: fill_rate={:.2}, slippage={:.2}, should_pause={}",
            quality.recent_fill_rate,
            quality.recent_avg_slippage,
            quality.should_pause_trading
        );
        if quality.should_pause_trading {
            return Some(format!(
                "Poor execution quality: fill_rate={:.1}%, avg_slippage={:.1}%",
                quality.recent_fill_rate * 100.0,
                quality.recent_avg_slippage
            ));
        }
        drop(exec_feedback);

        // Check portfolio circuit breaker
        let portfolio = self.portfolio_risk.read().await;
        let state = portfolio.get_state();
        tracing::debug!(
            "Portfolio check: positions={}, exposure={:.3}, can_open={}, reason={:?}",
            state.open_position_count,
            state.total_exposure_sol,
            state.can_open_new,
            state.reason_if_blocked
        );
        if !state.can_open_new {
            return Some(format!(
                "Portfolio blocked: {}",
                state.reason_if_blocked.as_deref().unwrap_or("unknown")
            ));
        }

        None
    }

    /// Check if trading should be paused (convenience method)
    pub async fn should_pause_trading(&self) -> bool {
        self.should_pause_trading_with_reason().await.is_some()
    }

    /// Get current portfolio state
    pub async fn get_portfolio_state(&self) -> super::portfolio_risk::PortfolioState {
        let portfolio = self.portfolio_risk.read().await;
        portfolio.get_state()
    }

    /// Get current chain state
    pub async fn get_chain_state(&self) -> super::chain_health::ChainState {
        let chain_health = self.chain_health.read().await;
        chain_health.get_state()
    }

    /// Get execution quality metrics
    pub async fn get_execution_quality(&self) -> super::execution_feedback::ExecutionQuality {
        let feedback = self.execution_feedback.read().await;
        feedback.get_quality()
    }

    /// Sample chain health (call periodically)
    pub async fn sample_chain_health(&self, rpc: &solana_client::nonblocking::rpc_client::RpcClient) {
        let mut chain_health = self.chain_health.write().await;
        chain_health.sample(rpc).await;
    }

    /// Mark a tiered exit level as hit
    pub async fn mark_exit_level_hit(&self, mint: &str, level: f64) {
        let mut exit_manager = self.exit_manager.write().await;
        exit_manager.mark_level_hit(mint, level);
    }

    // Helper methods

    fn get_or_create_delta_tracker(&mut self, mint: &str) -> &mut DeltaTracker {
        self.delta_trackers
            .entry(mint.to_string())
            .or_insert_with(DeltaTracker::new)
    }

    fn get_or_create_price_analyzer(&mut self, mint: &str) -> &mut PriceActionAnalyzer {
        self.price_analyzers
            .entry(mint.to_string())
            .or_insert_with(PriceActionAnalyzer::new)
    }

    fn get_current_price(&self, mint: &str) -> Option<f64> {
        self.price_analyzers
            .get(mint)
            .map(|a| a.analyze().current_price)
    }

    fn build_explanation(
        &self,
        ctx: &TokenAnalysisContext,
        regime: &RegimeClassification,
        position_size: f64,
        chain_state: &super::chain_health::ChainState,
    ) -> DecisionExplanation {
        DecisionExplanation {
            timestamp: chrono::Utc::now(),
            mint: ctx.mint.clone(),
            final_score: ctx.confidence_score,
            action: TradingAction::Hold,
            top_contributing_signals: vec![],
            top_risk_factors: vec![],
            regime: regime.regime.clone(),
            regime_confidence: regime.confidence,
            data_completeness: ctx.confidence_score,
            missing_data: vec![],
            selected_strategy: self.config.default_strategy.clone(),
            strategy_reason: regime.reasons.join(", "),
            position_size_sol: position_size,
            exit_style: super::types::ExitStyle::default(),
            decision_source: super::types::DecisionSource::Strategy,
            overridden_signals: vec![],
            open_position_count: 0,
            total_exposure_sol: 0.0,
            portfolio_block_reason: None,
            chain_congestion: chain_state.congestion_level,
            chain_action_taken: chain_state.recommended_action.clone(),
            recent_slippage_avg: 0.0,
            confidence_adjustment: 0.0,
            entry_delay_applied_ms: 0,
            size_jitter_applied_pct: 0.0,
        }
    }
}

impl Default for StrategyEngine {
    fn default() -> Self {
        Self::new(StrategyEngineConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::types::ExitStyle;

    #[tokio::test]
    async fn test_engine_creation() {
        let engine = StrategyEngine::default();
        assert!(engine.is_enabled());
    }

    #[tokio::test]
    async fn test_should_pause_default() {
        let engine = StrategyEngine::default();
        let should_pause = engine.should_pause_trading().await;
        assert!(!should_pause); // Should not pause by default
    }

    #[tokio::test]
    async fn test_portfolio_state() {
        let engine = StrategyEngine::default();
        let state = engine.get_portfolio_state().await;
        assert_eq!(state.open_position_count, 0);
        assert!(state.can_open_new);
    }

    #[tokio::test]
    async fn test_chain_state() {
        let engine = StrategyEngine::default();
        let state = engine.get_chain_state().await;
        assert!(matches!(
            state.congestion_level,
            super::super::types::CongestionLevel::Normal
        ));
    }

    #[tokio::test]
    async fn test_record_entry_exit() {
        let mut engine = StrategyEngine::default();

        let position = Position {
            mint: "test_mint".to_string(),
            entry_price: 0.001,
            entry_time: chrono::Utc::now(),
            size_sol: 0.1,
            tokens_held: 100_000,
            strategy: TradingStrategy::MomentumSurfing,
            exit_style: ExitStyle::default(),
            highest_price: 0.001,
            lowest_price: 0.001,
            exit_levels_hit: vec![],
        };

        engine.record_entry(position).await;

        let state = engine.get_portfolio_state().await;
        assert_eq!(state.open_position_count, 1);

        engine.record_exit("test_mint", 0.02).await; // 0.02 SOL profit

        let state = engine.get_portfolio_state().await;
        assert_eq!(state.open_position_count, 0);
    }

    #[tokio::test]
    async fn test_price_update() {
        let mut engine = StrategyEngine::default();

        engine.update_price("test_mint", 0.001, 1.0);
        engine.update_price("test_mint", 0.0012, 1.5);

        // Price analyzer should have data now
        assert!(engine.price_analyzers.contains_key("test_mint"));
    }

    #[tokio::test]
    async fn test_evaluate_entry_wash_trade() {
        let mut engine = StrategyEngine::default();

        let ctx = TokenAnalysisContext {
            mint: "wash_mint".to_string(),
            order_flow: OrderFlowAnalysis {
                wash_trading_score: 0.9, // High wash trading
                organic_score: 0.1,
                ..Default::default()
            },
            distribution: TokenDistribution::default(),
            creator_behavior: CreatorBehavior::default(),
            price_action: PriceAction::default(),
            sol_reserves: 100.0,
            token_reserves: 1_000_000.0,
            confidence_score: 0.8,
        };

        let evaluation = engine.evaluate_entry(&ctx).await;

        // Should reject due to wash trading
        assert!(matches!(
            evaluation.regime.regime,
            TokenRegime::WashTrade { .. }
        ));
        assert!(!evaluation.regime.should_enter);
    }
}
